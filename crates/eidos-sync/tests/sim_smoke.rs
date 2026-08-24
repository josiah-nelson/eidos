//! The harness's own acceptance tests: determinism, a correct protocol
//! surviving seeded fault storms with all invariants on, and two planted
//! bug classes being found by seed search and reduced by the shrinker.
//!
//! Seed counts default to a CI-friendly size; set EIDOS_SYNC_SEEDS to soak
//! (e.g. 1_000_000 for the fleet gate's overnight run).

use eidos_sync::env::Node;
use eidos_sync::rng::DeterministicRng;
use eidos_sync::shrink::{reproducer, shrink};
use eidos_sync::sim::{FaultPlan, Invariant, Simulation, Violation};
use eidos_sync::toy::{self, ToyCentral, ToyMsg, ToySource, CENTRAL, SOURCE};

const HORIZON_NS: u64 = 3_000_000_000;
const QUIET_NS: u64 = 4_000_000_000;
const MAX_STEPS: u64 = 200_000;
const TARGET: u64 = 40;

type Factory = Box<dyn Fn() -> Box<dyn Node<Msg = ToyMsg>>>;

fn factories(compact_before_ack: bool, durable_before_ack: bool) -> Vec<Factory> {
    vec![
        Box::new(move || Box::new(ToySource::new(TARGET, compact_before_ack)) as _),
        Box::new(move || Box::new(ToyCentral::new(durable_before_ack)) as _),
    ]
}

fn seeds() -> u64 {
    std::env::var("EIDOS_SYNC_SEEDS")
        .ok()
        .and_then(|s| s.replace('_', "").parse().ok())
        .unwrap_or(400)
}

fn plan_for(seed: u64) -> FaultPlan {
    FaultPlan::random(&mut DeterministicRng::new(seed), 2, HORIZON_NS)
}

fn run(
    seed: u64,
    plan: FaultPlan,
    compact_before_ack: bool,
    durable_before_ack: bool,
    all_invariants: bool,
) -> (Simulation<ToyMsg>, Result<(), Violation>) {
    let mut sim = Simulation::new(
        seed,
        plan,
        factories(compact_before_ack, durable_before_ack),
    )
    .expect("valid simulation plan");
    let mut invs: Vec<Invariant<'_, ToyMsg>> = vec![Box::new(toy::no_lost_rows)];
    if all_invariants {
        invs.push(Box::new(toy::compaction_below_ack));
        invs.push(Box::new(toy::watermark_monotonic()));
    }
    let outcome = sim.run_until(HORIZON_NS + QUIET_NS, MAX_STEPS, &mut invs);
    (sim, outcome)
}

fn central_watermark(sim: &Simulation<ToyMsg>) -> u64 {
    sim.durable(CENTRAL, "ctr")
        .map(|b| {
            serde_json::from_slice::<toy::CentralState>(b)
                .expect("central state")
                .watermark
        })
        .unwrap_or(0)
}

#[test]
fn same_seed_same_universe() {
    for seed in 0..20 {
        let (a, ra) = run(seed, plan_for(seed), false, true, true);
        let (b, rb) = run(seed, plan_for(seed), false, true, true);
        assert_eq!(ra.is_ok(), rb.is_ok());
        assert_eq!(a.fingerprint(), b.fingerprint(), "seed {seed} diverged");
        assert_eq!(a.stats().steps, b.stats().steps);
        assert_eq!(a.stats().dropped, b.stats().dropped);
    }
}

#[test]
fn correct_protocol_survives_fault_storms_and_converges() {
    let n = seeds();
    let mut faulty_runs = 0u64;
    for seed in 0..n {
        let plan = plan_for(seed);
        let had_faults = plan.drop_permille > 0 || !plan.events.is_empty();
        faulty_runs += u64::from(had_faults);
        let (sim, outcome) = run(seed, plan, false, true, true);
        if let Err(v) = outcome {
            panic!(
                "correct protocol violated an invariant\n  {}\n  at {} ns after {} steps\n  reproducer: {}",
                v.message,
                v.at_ns,
                v.steps,
                reproducer(seed, &plan_for(seed))
            );
        }
        // Liveness: after the network heals, everything converges.
        assert_eq!(
            central_watermark(&sim),
            TARGET,
            "seed {seed} did not converge: {}",
            reproducer(seed, &plan_for(seed))
        );
    }
    assert!(
        faulty_runs > n / 2,
        "fault plans were mostly benign ({faulty_runs}/{n}); the storm is not a storm"
    );
}

/// Bug class 1: compacting on ship instead of on ack. Harmless on a clean
/// network — rows ride in flight — and loses rows exactly when the network
/// drops the wrong message. The invariant must catch it, and the shrinker
/// must cut the plan down while keeping it failing.
#[test]
fn seed_search_finds_compact_before_ack_and_shrinks() {
    let mut found = None;
    for seed in 0..seeds() {
        let (_, outcome) = run(seed, plan_for(seed), true, true, false);
        if outcome.is_err() {
            found = Some(seed);
            break;
        }
    }
    let seed = found.expect("no seed exposed the compaction bug; the storm is too gentle");

    let still_fails =
        |candidate: &FaultPlan| run(seed, candidate.clone(), true, true, false).1.is_err();
    let minimal = shrink(plan_for(seed), still_fails);
    assert!(still_fails(&minimal));
    assert!(
        minimal.events.len() <= plan_for(seed).events.len(),
        "shrinking must never grow the plan"
    );
    // The bug needs message loss, not crashes: the shrunk plan should have
    // dropped every crash/restart event (loss may come from the drop knob or
    // a partition window, whichever this seed used).
    assert!(
        minimal.dup_permille == 0,
        "duplication cannot cause row loss; shrink should zero it: {}",
        reproducer(seed, &minimal)
    );
    println!("minimal reproducer: {}", reproducer(seed, &minimal));
}

/// Bug class 2: acking before the apply is durable. Invisible until a crash
/// discards the un-fsynced watermark — then rows the source already
/// compacted are gone. Detection therefore requires a plan that resets the
/// central process, and the shrunk plan must retain that reset.
#[test]
fn seed_search_finds_ack_before_durability_and_shrinks_to_the_crash() {
    let mut found = None;
    for seed in 0..seeds() {
        let plan = plan_for(seed);
        let crashes_central = plan.events.iter().any(
            |e| matches!(e, eidos_sync::sim::FaultEvent::Crash { node, .. } if *node == CENTRAL),
        );
        if !crashes_central {
            continue;
        }
        let (_, outcome) = run(seed, plan, false, false, true);
        if outcome.is_err() {
            found = Some(seed);
            break;
        }
    }
    let seed = found.expect("no seed crashed the central hard enough; widen the storm");

    let still_fails =
        |candidate: &FaultPlan| run(seed, candidate.clone(), false, false, true).1.is_err();
    let minimal = shrink(plan_for(seed), still_fails);
    assert!(
        minimal.events.iter().any(|e| matches!(
            e,
            eidos_sync::sim::FaultEvent::Crash { node, .. }
                | eidos_sync::sim::FaultEvent::Restart { node, .. }
                if *node == CENTRAL
        )),
        "a central process reset is the trigger; shrinking must keep it: {}",
        reproducer(seed, &minimal)
    );
    println!("minimal reproducer: {}", reproducer(seed, &minimal));
}

/// Crash/restart of the source must be recovered from durable state alone:
/// a benign-network plan with a forced source crash still converges.
#[test]
fn source_crash_recovers_from_durable_state() {
    let mut plan = FaultPlan::benign();
    plan.fault_horizon_ns = HORIZON_NS;
    plan.events = vec![
        eidos_sync::sim::FaultEvent::Crash {
            node: SOURCE,
            at_ns: 700_000_000,
        },
        eidos_sync::sim::FaultEvent::Restart {
            node: SOURCE,
            at_ns: 1_200_000_000,
        },
    ];
    let (sim, outcome) = run(7, plan, false, true, true);
    outcome.expect("crash recovery must not violate invariants");
    assert_eq!(central_watermark(&sim), TARGET);
}
