//! Direct contracts for the DST runtime itself. Random protocol tests depend
//! on these semantics and should not be the only evidence that faults work.

use eidos_sync::env::{Env, Node, NodeId};
use eidos_sync::sim::{FaultEvent, FaultPlan, Invariant, NodeFactory, PlanError, Simulation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
enum Msg {
    Ping,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Counts {
    starts: u64,
    timers: u64,
    messages: u64,
    last_time: u64,
}

fn read_counts(env: &mut dyn Env<Msg>) -> Counts {
    env.fs()
        .read("counts")
        .map(|bytes| serde_json::from_slice(bytes).unwrap())
        .unwrap_or_default()
}

fn write_counts(env: &mut dyn Env<Msg>, counts: &Counts) {
    env.fs()
        .write_durable("counts", serde_json::to_vec(counts).unwrap());
}

struct CountingNode {
    timer_ns: Option<u64>,
}

impl Node for CountingNode {
    type Msg = Msg;

    fn on_start(&mut self, env: &mut dyn Env<Msg>) {
        let mut counts = read_counts(env);
        counts.starts += 1;
        write_counts(env, &counts);
        if let Some(after) = self.timer_ns {
            env.set_timer(after, 1);
        }
    }

    fn on_message(&mut self, env: &mut dyn Env<Msg>, _from: NodeId, _msg: Msg) {
        let mut counts = read_counts(env);
        counts.messages += 1;
        counts.last_time = env.now().0;
        write_counts(env, &counts);
    }

    fn on_timer(&mut self, env: &mut dyn Env<Msg>, _timer: u32) {
        let mut counts = read_counts(env);
        counts.timers += 1;
        write_counts(env, &counts);
    }
}

struct SendOnStart {
    to: NodeId,
}

struct SendAcrossWindow {
    to: NodeId,
}

impl Node for SendAcrossWindow {
    type Msg = Msg;

    fn on_start(&mut self, env: &mut dyn Env<Msg>) {
        let mut state = read_counts(env);
        state.starts += 1;
        write_counts(env, &state);
        env.send(self.to, Msg::Ping);
        env.set_timer(5, 1);
    }
    fn on_message(&mut self, env: &mut dyn Env<Msg>, _from: NodeId, _msg: Msg) {
        let mut state = read_counts(env);
        state.messages += 1;
        write_counts(env, &state);
    }
    fn on_timer(&mut self, env: &mut dyn Env<Msg>, _timer: u32) {
        env.send(self.to, Msg::Ping);
    }
}

impl Node for SendOnStart {
    type Msg = Msg;

    fn on_start(&mut self, env: &mut dyn Env<Msg>) {
        env.send(self.to, Msg::Ping);
    }
    fn on_message(&mut self, _env: &mut dyn Env<Msg>, _from: NodeId, _msg: Msg) {}
    fn on_timer(&mut self, _env: &mut dyn Env<Msg>, _timer: u32) {}
}

fn empty_invariants() -> Vec<Invariant<'static, Msg>> {
    Vec::new()
}

fn counts(sim: &Simulation<Msg>, node: NodeId) -> Counts {
    serde_json::from_slice(sim.durable(node, "counts").unwrap()).unwrap()
}

#[test]
fn run_until_is_resumable_and_starts_nodes_once() {
    let factories: Vec<NodeFactory<Msg>> = vec![Box::new(|| {
        Box::new(CountingNode { timer_ns: Some(10) }) as _
    })];
    let mut sim = Simulation::new(1, FaultPlan::benign(), factories).unwrap();
    let mut invariants = empty_invariants();
    sim.run_until(5, 10, &mut invariants).unwrap();
    assert_eq!(sim.stats().steps, 0);
    sim.run_until(20, 10, &mut invariants).unwrap();
    let counts = counts(&sim, 0);
    assert_eq!(counts.starts, 1);
    assert_eq!(counts.timers, 1);
}

#[test]
fn standalone_restart_discards_ram_and_cancels_old_timers() {
    let mut plan = FaultPlan::benign();
    plan.events.push(FaultEvent::Restart { node: 0, at_ns: 5 });
    let factories: Vec<NodeFactory<Msg>> = vec![Box::new(|| {
        Box::new(CountingNode { timer_ns: Some(10) }) as _
    })];
    let mut sim = Simulation::new(2, plan, factories).unwrap();
    sim.run_until(11, 20, &mut empty_invariants()).unwrap();
    let state = counts(&sim, 0);
    assert_eq!(state.starts, 2);
    assert_eq!(state.timers, 0, "the pre-restart timer must be stale");
    sim.run_until(16, 20, &mut empty_invariants()).unwrap();
    assert_eq!(counts(&sim, 0).timers, 1);
}

#[test]
fn fixed_delay_is_inclusive_and_exact() {
    let mut plan = FaultPlan::benign();
    plan.min_delay_ns = 7;
    plan.max_delay_ns = 7;
    let factories: Vec<NodeFactory<Msg>> = vec![
        Box::new(|| Box::new(SendOnStart { to: 1 }) as _),
        Box::new(|| Box::new(CountingNode { timer_ns: None }) as _),
    ];
    let mut sim = Simulation::new(3, plan, factories).unwrap();
    sim.run_until(6, 10, &mut empty_invariants()).unwrap();
    assert_eq!(counts(&sim, 1).messages, 0);
    sim.run_until(7, 10, &mut empty_invariants()).unwrap();
    assert_eq!(counts(&sim, 1).last_time, 7);
}

#[test]
fn duplication_delivers_two_copies_and_accounts_for_it() {
    let mut plan = FaultPlan::benign();
    plan.dup_permille = 1000;
    plan.fault_horizon_ns = 10;
    plan.min_delay_ns = 1;
    plan.max_delay_ns = 1;
    let factories: Vec<NodeFactory<Msg>> = vec![
        Box::new(|| Box::new(SendOnStart { to: 1 }) as _),
        Box::new(|| Box::new(CountingNode { timer_ns: None }) as _),
    ];
    let mut sim = Simulation::new(4, plan, factories).unwrap();
    sim.run_until(2, 10, &mut empty_invariants()).unwrap();
    assert_eq!(counts(&sim, 1).messages, 2);
    assert_eq!(sim.stats().duplicated, 1);
    assert_eq!(sim.stats().delivered, 2);
}

#[test]
fn partition_is_bidirectional_and_heals_at_the_horizon() {
    let mut plan = FaultPlan::benign();
    plan.fault_horizon_ns = 5;
    plan.min_delay_ns = 1;
    plan.max_delay_ns = 1;
    plan.events.push(FaultEvent::Partition {
        a: 0,
        b: 1,
        from_ns: 0,
        until_ns: 5,
    });
    let factories: Vec<NodeFactory<Msg>> = vec![
        Box::new(|| Box::new(SendAcrossWindow { to: 1 }) as _),
        Box::new(|| Box::new(SendAcrossWindow { to: 0 }) as _),
    ];
    let mut sim = Simulation::new(8, plan, factories).unwrap();
    sim.run_until(7, 20, &mut empty_invariants()).unwrap();
    assert_eq!(counts(&sim, 0).messages, 1);
    assert_eq!(counts(&sim, 1).messages, 1);
    assert_eq!(sim.stats().dropped, 2);
}

#[test]
fn invalid_plans_and_node_outputs_fail_cleanly() {
    for horizon in 0..10 {
        let plan = FaultPlan::random(
            &mut eidos_sync::rng::DeterministicRng::new(horizon),
            2,
            horizon,
        );
        plan.validate(2).unwrap();
    }

    let mut invalid = FaultPlan::benign();
    invalid.drop_permille = 1001;
    assert!(matches!(
        Simulation::<Msg>::new(1, invalid, Vec::new()),
        Err(PlanError::Probability { .. })
    ));

    let factories: Vec<NodeFactory<Msg>> = vec![Box::new(|| Box::new(SendOnStart { to: 99 }) as _)];
    let mut sim = Simulation::new(5, FaultPlan::benign(), factories).unwrap();
    let violation = sim.run_until(1, 10, &mut empty_invariants()).unwrap_err();
    assert!(violation.message.contains("nonexistent node 99"));
}
