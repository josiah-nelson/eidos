//! S1B acceptance: the real shipper/applier skeleton runs inside DST before
//! any socket exists.

use eidos_domain::{ObjectId, SourceId};
use eidos_sync::env::Node;
use eidos_sync::identity::SourceEpoch;
use eidos_sync::protocol::{
    self, Applier, CursorState, LocalMutation, MaterializedRow, Outbox, ReplicaState, Shipper,
    ShipperState, SyncMsg, CURSOR_STATE_KEY, REPLICA_STATE_KEY, SHIPPER_STATE_KEY,
};
use eidos_sync::rng::DeterministicRng;
use eidos_sync::shrink::reproducer;
use eidos_sync::sim::{FaultEvent, FaultPlan, Invariant, NodeFactory, Simulation, Violation};
use std::collections::BTreeMap;

const SOURCE_NODE: usize = 0;
const CENTRAL_NODE: usize = 1;
const SOURCE_ID: SourceId = SourceId::new(41);
const HORIZON_NS: u64 = 2_000_000_000;
const QUIET_NS: u64 = 3_000_000_000;
const MAX_STEPS: u64 = 100_000;

const EPOCH_SEED: u64 = 0x1234;

fn epoch() -> SourceEpoch {
    SourceEpoch::random_v4(EPOCH_SEED, 0)
}

fn workload() -> Vec<LocalMutation> {
    let mut generations = [0u64; 10];
    let mut mutations = Vec::new();
    for index in 0..80usize {
        let object = index % generations.len();
        generations[object] += 1;
        mutations.push(LocalMutation {
            object: ObjectId::new(object as i64 + 1),
            generation: generations[object],
            value: Some(format!("object-{object}-version-{}", generations[object]).into_bytes()),
        });
    }
    for object in [1usize, 4, 8] {
        generations[object] += 1;
        mutations.push(LocalMutation {
            object: ObjectId::new(object as i64 + 1),
            generation: generations[object],
            value: None,
        });
    }
    mutations
}

fn expected_rows(script: &[LocalMutation]) -> BTreeMap<ObjectId, MaterializedRow> {
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

fn factories(script: Vec<LocalMutation>, durable_before_ack: bool) -> Vec<NodeFactory<SyncMsg>> {
    vec![
        Box::new(move || {
            Box::new(
                Shipper::from_mutations(SOURCE_ID, EPOCH_SEED, CENTRAL_NODE, script.clone())
                    .with_repair_leaf_bits(8),
            ) as Box<dyn Node<Msg = SyncMsg>>
        }),
        Box::new(move || Box::new(Applier::new(durable_before_ack)) as _),
    ]
}

fn tombstones(script: &[LocalMutation]) -> Vec<(u64, ObjectId)> {
    script
        .iter()
        .enumerate()
        .filter(|(_, mutation)| mutation.value.is_none())
        .map(|(index, mutation)| (index as u64 + 1, mutation.object))
        .collect()
}

fn run(
    seed: u64,
    plan: FaultPlan,
    durable_before_ack: bool,
) -> (Simulation<SyncMsg>, Result<(), Violation>) {
    let script = workload();
    let mut sim = Simulation::new(seed, plan, factories(script.clone(), durable_before_ack))
        .expect("valid protocol fault plan");
    let mut invariants: Vec<Invariant<'_, SyncMsg>> = vec![
        Box::new(protocol::compaction_respects_oldest_watermark(SOURCE_NODE)),
        Box::new(protocol::watermark_monotonic(CENTRAL_NODE, SOURCE_ID)),
        Box::new(protocol::effects_do_not_lead_watermarks(CENTRAL_NODE)),
        Box::new(protocol::no_lost_tombstones(
            SOURCE_NODE,
            CENTRAL_NODE,
            SOURCE_ID,
            tombstones(&script),
        )),
    ];
    let result = sim.run_until(HORIZON_NS + QUIET_NS, MAX_STEPS, &mut invariants);
    (sim, result)
}

fn durable<T: serde::de::DeserializeOwned>(sim: &Simulation<SyncMsg>, node: usize, key: &str) -> T {
    serde_json::from_slice(sim.durable(node, key).expect("durable state")).expect("valid state")
}

fn assert_converged(sim: &Simulation<SyncMsg>) {
    let script = workload();
    let replicas: ReplicaState = durable(sim, CENTRAL_NODE, REPLICA_STATE_KEY);
    let cursors: CursorState = durable(sim, CENTRAL_NODE, CURSOR_STATE_KEY);
    let shipper: ShipperState = durable(sim, SOURCE_NODE, SHIPPER_STATE_KEY);
    assert_eq!(
        replicas.sources.get(&SOURCE_ID),
        Some(&expected_rows(&script))
    );
    assert_eq!(
        cursors.sources.get(&SOURCE_ID).unwrap().applied_seq,
        script.len() as u64
    );
    assert_eq!(shipper.outbox.compacted_through, script.len() as u64);
    assert!(shipper.outbox.changes().is_empty());
}

fn seeds() -> u64 {
    std::env::var("EIDOS_SYNC_PROTOCOL_SEEDS")
        .ok()
        .and_then(|value| value.replace('_', "").parse().ok())
        .unwrap_or(40)
}

#[test]
fn materialized_protocol_survives_fault_storms_and_converges() {
    for seed in 0..seeds() {
        let plan = FaultPlan::random(&mut DeterministicRng::new(seed), 2, HORIZON_NS);
        let (sim, outcome) = run(seed, plan.clone(), true);
        if let Err(violation) = outcome {
            panic!(
                "protocol violation: {violation}\nreproducer: {}",
                reproducer(seed, &plan)
            );
        }
        assert_converged(&sim);
    }
}

#[test]
fn simulated_three_week_offline_reconnect_uses_cursor_and_compacted_suffix() {
    const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
    const THREE_WEEKS_NS: u64 = 21 * DAY_NS;
    const SIX_HOURS_NS: u64 = 6 * 60 * 60 * 1_000_000_000;

    let script = workload();
    let source_script = script.clone();
    let protocol_factories: Vec<NodeFactory<SyncMsg>> = vec![
        Box::new(move || {
            Box::new(
                Shipper::from_mutations(SOURCE_ID, EPOCH_SEED, CENTRAL_NODE, source_script.clone())
                    .with_tick_ns(SIX_HOURS_NS),
            ) as _
        }),
        Box::new(|| Box::new(Applier::new(true)) as _),
    ];
    let mut plan = FaultPlan::benign();
    plan.fault_horizon_ns = THREE_WEEKS_NS;
    plan.events.push(FaultEvent::Partition {
        a: SOURCE_NODE,
        b: CENTRAL_NODE,
        from_ns: 1,
        until_ns: THREE_WEEKS_NS,
    });
    let mut sim = Simulation::new(17, plan, protocol_factories).unwrap();
    sim.trace = Some(Vec::new());
    let mut invariants: Vec<Invariant<'_, SyncMsg>> = vec![
        Box::new(protocol::compaction_respects_oldest_watermark(SOURCE_NODE)),
        Box::new(protocol::watermark_monotonic(CENTRAL_NODE, SOURCE_ID)),
        Box::new(protocol::no_lost_tombstones(
            SOURCE_NODE,
            CENTRAL_NODE,
            SOURCE_ID,
            tombstones(&script),
        )),
    ];
    sim.run_until(THREE_WEEKS_NS + 2 * DAY_NS, MAX_STEPS, &mut invariants)
        .unwrap();
    assert_converged(&sim);
    let trace = sim.trace.as_ref().unwrap();
    assert!(trace.iter().any(|line| line.contains("Resume {")));
    assert!(trace.iter().any(|line| line.contains("Batch(")));
    assert!(
        trace
            .iter()
            .all(|line| !line.contains("Snapshot(") && !line.contains("RepairOffer(")),
        "covered cursor reconnect must not fall back to a snapshot/re-crawl"
    );
}

#[test]
fn unsafe_ack_is_exposed_by_a_central_process_reset() {
    let mut found = None;
    for seed in 0..seeds() {
        let plan = FaultPlan::random(&mut DeterministicRng::new(seed), 2, HORIZON_NS);
        if !plan
            .events
            .iter()
            .any(|event| matches!(event, FaultEvent::Crash { node, .. } if *node == CENTRAL_NODE))
        {
            continue;
        }
        if run(seed, plan, false).1.is_err() {
            found = Some(seed);
            break;
        }
    }
    assert!(
        found.is_some(),
        "fault search failed to expose unsafe ACK ordering"
    );
}

#[test]
fn truncated_log_repairs_divergent_merkle_leaves_without_a_recrawl() {
    let script = workload();
    let mut outbox = Outbox::new([CENTRAL_NODE]);
    for mutation in script {
        outbox.append(mutation).unwrap();
    }
    let head = outbox.next_seq;
    outbox.acknowledge(CENTRAL_NODE, head).unwrap();
    let expected = outbox.rows().clone();
    let initial = ShipperState {
        source: SOURCE_ID,
        epoch: epoch(),
        outbox,
        incarnation: 0,
        script_index: 0,
        checkpoint: None,
    };
    let factories: Vec<NodeFactory<SyncMsg>> = vec![
        Box::new(move || {
            Box::new(Shipper::from_state(initial.clone(), CENTRAL_NODE).with_repair_leaf_bits(8))
                as _
        }),
        Box::new(|| Box::new(Applier::new(true)) as _),
    ];
    let mut sim = Simulation::new(91, FaultPlan::benign(), factories).unwrap();
    sim.trace = Some(Vec::new());
    let mut invariants: Vec<Invariant<'_, SyncMsg>> = vec![
        Box::new(protocol::compaction_respects_oldest_watermark(SOURCE_NODE)),
        Box::new(protocol::watermark_monotonic(CENTRAL_NODE, SOURCE_ID)),
        Box::new(protocol::effects_do_not_lead_watermarks(CENTRAL_NODE)),
    ];
    sim.run_until(1_000_000_000, MAX_STEPS, &mut invariants)
        .unwrap();
    let replicas: ReplicaState = durable(&sim, CENTRAL_NODE, REPLICA_STATE_KEY);
    let cursors: CursorState = durable(&sim, CENTRAL_NODE, CURSOR_STATE_KEY);
    assert_eq!(replicas.sources.get(&SOURCE_ID), Some(&expected));
    assert_eq!(cursors.sources.get(&SOURCE_ID).unwrap().applied_seq, head);
    let trace = sim.trace.as_ref().unwrap();
    assert!(trace.iter().any(|line| line.contains("RepairRows(")));
    assert!(trace.iter().all(|line| !line.contains("Snapshot(")));
}
