//! Seeded, single-threaded, fault-injecting simulation.
//!
//! One event heap drives every node callback; all nondeterminism flows from
//! one [`DeterministicRng`] seed, so a failing run is a `(seed, plan)` pair
//! that reproduces exactly, forever. Faults: message drop, duplication,
//! random delay, pairwise partitions, and crash/restart with RAM and timers
//! lost and un-fsynced storage discarded. Invariant hooks run after every
//! event and see only node storage — the state that would exist in reality.

use crate::env::{Clock, Env, Fs, Node, NodeId, SimFs, SimTime, Timers, Transport};
use crate::rng::DeterministicRng;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FaultEvent {
    /// RAM and timers lost; un-fsynced storage discarded.
    Crash { node: NodeId, at_ns: u64 },
    /// Fresh instance built by the node factory; `on_start` recovers.
    Restart { node: NodeId, at_ns: u64 },
    /// Messages between `a` and `b` (both directions) sent inside the
    /// window are dropped.
    Partition {
        a: NodeId,
        b: NodeId,
        from_ns: u64,
        until_ns: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultPlan {
    /// Probability (per mille) that a send is dropped, before the horizon.
    pub drop_permille: u32,
    /// Probability (per mille) that a send is duplicated.
    pub dup_permille: u32,
    pub min_delay_ns: u64,
    pub max_delay_ns: u64,
    /// After this instant the network heals: no drops, duplicates, or
    /// partitions (delays remain), so liveness can be asserted at the end.
    pub fault_horizon_ns: u64,
    pub events: Vec<FaultEvent>,
}

impl FaultPlan {
    pub fn benign() -> Self {
        Self {
            drop_permille: 0,
            dup_permille: 0,
            min_delay_ns: 1_000_000,
            max_delay_ns: 5_000_000,
            fault_horizon_ns: 0,
            events: Vec::new(),
        }
    }

    /// Draw a random plan: lossy/duplicating network, up to one partition
    /// window, and up to one crash/restart pair per node, all before the
    /// horizon.
    pub fn random(rng: &mut DeterministicRng, nodes: usize, horizon_ns: u64) -> Self {
        let mut events = Vec::new();
        for node in 0..nodes {
            if horizon_ns >= 2 && rng.chance(400) {
                // Leave at least one nanosecond for a restart before the
                // fault horizon. Keeping this valid for tiny horizons makes
                // boundary/property tests able to generate plans freely.
                let crash_at = rng.below(horizon_ns - 1);
                let remaining = horizon_ns - crash_at - 1;
                let downtime = 1 + rng.below(remaining);
                events.push(FaultEvent::Crash {
                    node,
                    at_ns: crash_at,
                });
                events.push(FaultEvent::Restart {
                    node,
                    at_ns: crash_at + downtime,
                });
            }
        }
        if nodes >= 2 && horizon_ns > 0 && rng.chance(300) {
            let a = rng.below(nodes as u64) as NodeId;
            let b = (a + 1 + rng.below(nodes as u64 - 1) as NodeId) % nodes;
            let from_ns = rng.below(horizon_ns);
            let remaining = horizon_ns - from_ns;
            events.push(FaultEvent::Partition {
                a,
                b,
                from_ns,
                until_ns: from_ns + 1 + rng.below(remaining),
            });
        }
        Self {
            drop_permille: rng.below(250) as u32,
            dup_permille: rng.below(120) as u32,
            min_delay_ns: 500_000,
            max_delay_ns: 1_000_000 + rng.below(30_000_000),
            fault_horizon_ns: horizon_ns,
            events,
        }
    }

    /// Validate a manually assembled plan against a simulation topology.
    pub fn validate(&self, nodes: usize) -> Result<(), PlanError> {
        if self.drop_permille > 1000 {
            return Err(PlanError::Probability {
                field: "drop_permille",
                value: self.drop_permille,
            });
        }
        if self.dup_permille > 1000 {
            return Err(PlanError::Probability {
                field: "dup_permille",
                value: self.dup_permille,
            });
        }
        if self.min_delay_ns == 0 {
            return Err(PlanError::ZeroDelay);
        }
        if self.min_delay_ns > self.max_delay_ns {
            return Err(PlanError::DelayRange {
                min: self.min_delay_ns,
                max: self.max_delay_ns,
            });
        }
        for (index, event) in self.events.iter().enumerate() {
            match event {
                FaultEvent::Crash { node, .. } | FaultEvent::Restart { node, .. } => {
                    if *node >= nodes {
                        return Err(PlanError::Node {
                            event: index,
                            node: *node,
                            nodes,
                        });
                    }
                }
                FaultEvent::Partition {
                    a,
                    b,
                    from_ns,
                    until_ns,
                } => {
                    if *a >= nodes {
                        return Err(PlanError::Node {
                            event: index,
                            node: *a,
                            nodes,
                        });
                    }
                    if *b >= nodes {
                        return Err(PlanError::Node {
                            event: index,
                            node: *b,
                            nodes,
                        });
                    }
                    if a == b {
                        return Err(PlanError::SelfPartition {
                            event: index,
                            node: *a,
                        });
                    }
                    if from_ns >= until_ns {
                        return Err(PlanError::PartitionWindow {
                            event: index,
                            from: *from_ns,
                            until: *until_ns,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    #[error("{field} must be in 0..=1000, got {value}")]
    Probability { field: &'static str, value: u32 },
    #[error("network delay must be at least one nanosecond")]
    ZeroDelay,
    #[error("invalid network delay range {min}..{max}")]
    DelayRange { min: u64, max: u64 },
    #[error("fault event {event} references node {node}, but the simulation has {nodes} nodes")]
    Node {
        event: usize,
        node: NodeId,
        nodes: usize,
    },
    #[error("fault event {event} partitions node {node} from itself")]
    SelfPartition { event: usize, node: NodeId },
    #[error("fault event {event} has an empty/inverted partition window {from}..{until}")]
    PartitionWindow { event: usize, from: u64, until: u64 },
}

enum Kind<M> {
    Deliver {
        to: NodeId,
        from: NodeId,
        msg: M,
    },
    Timer {
        node: NodeId,
        timer: u32,
        epoch: u32,
    },
    Crash {
        node: NodeId,
    },
    Restart {
        node: NodeId,
    },
}

struct Scheduled<M> {
    at_ns: u64,
    seq: u64,
    kind: Kind<M>,
}

impl<M> PartialEq for Scheduled<M> {
    fn eq(&self, other: &Self) -> bool {
        self.at_ns == other.at_ns && self.seq == other.seq
    }
}
impl<M> Eq for Scheduled<M> {}
impl<M> PartialOrd for Scheduled<M> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<M> Ord for Scheduled<M> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at_ns, self.seq).cmp(&(other.at_ns, other.seq))
    }
}

/// What invariants may look at: node storage plus the messages currently in
/// flight. Storage rather than node RAM keeps invariants honest about what
/// actually exists; `durable` is what a crash right now would leave behind,
/// and `in_flight` lets an invariant treat an undelivered message as a
/// (droppable) copy — so a state is flagged the moment a fault makes it
/// bad, not merely when it could later become bad.
pub struct InvariantCtx<'a, M> {
    pub now: SimTime,
    fs: &'a [SimFs],
    queued: &'a BinaryHeap<Reverse<Scheduled<M>>>,
}

impl<M> InvariantCtx<'_, M> {
    pub fn durable(&self, node: NodeId, key: &str) -> Option<&[u8]> {
        self.fs[node].durable(key)
    }
    pub fn buffered(&self, node: NodeId, key: &str) -> Option<&[u8]> {
        self.fs[node].read(key)
    }
    /// Undelivered messages, in no particular order.
    pub fn in_flight(&self) -> impl Iterator<Item = (NodeId, NodeId, &M)> {
        self.queued.iter().filter_map(|Reverse(s)| match &s.kind {
            Kind::Deliver { to, from, msg } => Some((*from, *to, msg)),
            _ => None,
        })
    }
}

pub type Invariant<'a, M> = Box<dyn FnMut(&InvariantCtx<'_, M>) -> Result<(), String> + 'a>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message} at {at_ns}ns after {steps} steps")]
pub struct Violation {
    pub message: String,
    pub at_ns: u64,
    pub steps: u64,
}

#[derive(Debug, Default)]
pub struct RunStats {
    pub steps: u64,
    pub delivered: u64,
    pub dropped: u64,
    pub duplicated: u64,
}

/// Environment handed to one node callback: outputs are collected and
/// scheduled by the simulation after the callback returns.
struct CallbackEnv<'a, M> {
    now: SimTime,
    fs: &'a mut SimFs,
    out_msgs: Vec<(NodeId, M)>,
    out_timers: Vec<(u64, u32)>,
}

impl<M> Clock for CallbackEnv<'_, M> {
    fn now(&self) -> SimTime {
        self.now
    }
}

impl<M> Transport<M> for CallbackEnv<'_, M> {
    fn send(&mut self, to: NodeId, msg: M) {
        self.out_msgs.push((to, msg));
    }
}

impl<M> Timers for CallbackEnv<'_, M> {
    fn set_timer(&mut self, after_ns: u64, timer: u32) {
        // A zero-delay timer would allow an infinite same-instant loop.
        self.out_timers.push((after_ns.max(1), timer));
    }
}

impl<M> Env<M> for CallbackEnv<'_, M> {
    fn fs(&mut self) -> &mut dyn Fs {
        self.fs
    }
}

pub type NodeFactory<M> = Box<dyn Fn() -> Box<dyn Node<Msg = M>>>;

pub struct Simulation<M: Clone + std::fmt::Debug> {
    rng: DeterministicRng,
    plan: FaultPlan,
    heap: BinaryHeap<Reverse<Scheduled<M>>>,
    seq: u64,
    now_ns: u64,
    nodes: Vec<Box<dyn Node<Msg = M>>>,
    factories: Vec<NodeFactory<M>>,
    fs: Vec<SimFs>,
    up: Vec<bool>,
    timer_epoch: Vec<u32>,
    stats: RunStats,
    started: bool,
    /// Human-readable event log, kept only when enabled (shrink reruns).
    pub trace: Option<Vec<String>>,
}

impl<M: Clone + std::fmt::Debug> Simulation<M> {
    pub fn new(
        seed: u64,
        plan: FaultPlan,
        factories: Vec<NodeFactory<M>>,
    ) -> Result<Self, PlanError> {
        let n = factories.len();
        plan.validate(n)?;
        let nodes = factories.iter().map(|f| f()).collect();
        let mut sim = Self {
            rng: DeterministicRng::new(seed),
            heap: BinaryHeap::new(),
            seq: 0,
            now_ns: 0,
            nodes,
            factories,
            fs: vec![SimFs::default(); n],
            up: vec![true; n],
            timer_epoch: vec![0; n],
            stats: RunStats::default(),
            started: false,
            trace: None,
            plan,
        };
        for e in sim.plan.events.clone() {
            match e {
                FaultEvent::Crash { node, at_ns } => sim.push(at_ns, Kind::Crash { node }),
                FaultEvent::Restart { node, at_ns } => sim.push(at_ns, Kind::Restart { node }),
                FaultEvent::Partition { .. } => {} // window checked at send time
            }
        }
        Ok(sim)
    }

    fn push(&mut self, at_ns: u64, kind: Kind<M>) {
        self.seq += 1;
        self.heap.push(Reverse(Scheduled {
            at_ns,
            seq: self.seq,
            kind,
        }));
    }

    fn note(&mut self, line: impl FnOnce() -> String) {
        if let Some(t) = self.trace.as_mut() {
            t.push(line());
        }
    }

    fn partitioned(&self, a: NodeId, b: NodeId, at_ns: u64) -> bool {
        if at_ns >= self.plan.fault_horizon_ns {
            return false;
        }
        self.plan.events.iter().any(|e| match e {
            FaultEvent::Partition {
                a: pa,
                b: pb,
                from_ns,
                until_ns,
            } => {
                ((a, b) == (*pa, *pb) || (b, a) == (*pa, *pb))
                    && at_ns >= *from_ns
                    && at_ns < *until_ns
            }
            _ => false,
        })
    }

    /// Run one node callback and schedule its outputs.
    fn callback(
        &mut self,
        node: NodeId,
        run: impl FnOnce(&mut dyn Node<Msg = M>, &mut CallbackEnv<'_, M>),
    ) -> Result<(), Violation> {
        let mut env = CallbackEnv {
            now: SimTime(self.now_ns),
            fs: &mut self.fs[node],
            out_msgs: Vec::new(),
            out_timers: Vec::new(),
        };
        run(self.nodes[node].as_mut(), &mut env);
        let CallbackEnv {
            out_msgs,
            out_timers,
            ..
        } = env;
        for (after_ns, timer) in out_timers {
            let epoch = self.timer_epoch[node];
            let at = self.now_ns.checked_add(after_ns).ok_or_else(|| {
                self.violation(format!(
                    "node {node} scheduled timer {timer} past the simulated time range"
                ))
            })?;
            self.push(at, Kind::Timer { node, timer, epoch });
        }
        for (to, msg) in out_msgs {
            self.transmit(node, to, msg)?;
        }
        Ok(())
    }

    fn transmit(&mut self, from: NodeId, to: NodeId, msg: M) -> Result<(), Violation> {
        if to >= self.nodes.len() {
            return Err(self.violation(format!(
                "node {from} sent a message to nonexistent node {to}"
            )));
        }
        let faulty = self.now_ns < self.plan.fault_horizon_ns;
        let now = self.now_ns;
        if self.partitioned(from, to, now) {
            self.stats.dropped += 1;
            self.note(|| format!("{now}ns drop(partition) {from}->{to}"));
            return Ok(());
        }
        if faulty && self.rng.chance(self.plan.drop_permille) {
            self.stats.dropped += 1;
            self.note(|| format!("{now}ns drop(loss) {from}->{to}"));
            return Ok(());
        }
        let copies = if faulty && self.rng.chance(self.plan.dup_permille) {
            self.stats.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let spread = self.plan.max_delay_ns - self.plan.min_delay_ns;
            let offset = match spread {
                0 => 0,
                u64::MAX => self.rng.next_u64(),
                _ => self.rng.below(spread + 1),
            };
            let delay = self.plan.min_delay_ns + offset;
            let at = self.now_ns.checked_add(delay).ok_or_else(|| {
                self.violation(format!(
                    "message {from}->{to} was delayed past the simulated time range"
                ))
            })?;
            self.push(
                at,
                Kind::Deliver {
                    to,
                    from,
                    msg: msg.clone(),
                },
            );
        }
        Ok(())
    }

    fn violation(&self, message: String) -> Violation {
        Violation {
            message,
            at_ns: self.now_ns,
            steps: self.stats.steps,
        }
    }

    /// Run until the heap drains past `until_ns` or `max_steps` events ran.
    /// Invariants are checked after every event; the first violation stops
    /// the run.
    pub fn run_until(
        &mut self,
        until_ns: u64,
        max_steps: u64,
        invariants: &mut [Invariant<'_, M>],
    ) -> Result<(), Violation> {
        if !self.started {
            self.started = true;
            // A lifecycle event at time zero defines the node's initial
            // process state. Let that event perform (or suppress) startup so
            // an immediate crash cannot create phantom durable writes or
            // messages before the modeled process ever ran.
            let lifecycle_at_zero: Vec<bool> = (0..self.nodes.len())
                .map(|node| {
                    self.plan.events.iter().any(|event| {
                        matches!(
                            event,
                            FaultEvent::Crash {
                                node: event_node,
                                at_ns: 0
                            } | FaultEvent::Restart {
                                node: event_node,
                                at_ns: 0
                            } if *event_node == node
                        )
                    })
                })
                .collect();
            for (node, managed_by_event) in lifecycle_at_zero.into_iter().enumerate() {
                if !managed_by_event {
                    self.callback(node, |n, env| n.on_start(env))?;
                }
            }
            self.check(invariants)?;
        }
        let step_limit = self.stats.steps.saturating_add(max_steps);
        while let Some(Reverse(next)) = self.heap.peek() {
            if next.at_ns > until_ns {
                break;
            }
            if self.stats.steps >= step_limit {
                return Err(Violation {
                    message: format!("step budget exhausted ({max_steps}); runaway event loop?"),
                    at_ns: self.now_ns,
                    steps: self.stats.steps,
                });
            }
            let Reverse(ev) = self.heap.pop().expect("peeked");
            self.now_ns = ev.at_ns;
            self.stats.steps += 1;
            match ev.kind {
                Kind::Deliver { to, from, msg } => {
                    if !self.up[to] {
                        self.stats.dropped += 1;
                        self.note(|| format!("{}ns drop(down) {from}->{to} {msg:?}", ev.at_ns));
                    } else {
                        self.stats.delivered += 1;
                        self.note(|| format!("{}ns deliver {from}->{to} {msg:?}", ev.at_ns));
                        self.callback(to, |n, env| n.on_message(env, from, msg))?;
                    }
                }
                Kind::Timer { node, timer, epoch } => {
                    if self.up[node] && self.timer_epoch[node] == epoch {
                        self.note(|| format!("{}ns timer {node}#{timer}", ev.at_ns));
                        self.callback(node, |n, env| n.on_timer(env, timer))?;
                    }
                }
                Kind::Crash { node } => {
                    self.note(|| format!("{}ns crash {node}", ev.at_ns));
                    self.up[node] = false;
                    self.timer_epoch[node] = self.timer_epoch[node].wrapping_add(1);
                    self.fs[node].crash();
                }
                Kind::Restart { node } => {
                    self.note(|| format!("{}ns restart {node}", ev.at_ns));
                    // A standalone restart is still a process restart: RAM,
                    // timers, and un-fsynced storage may not survive merely
                    // because a separate Crash event was omitted.
                    self.timer_epoch[node] = self.timer_epoch[node].wrapping_add(1);
                    self.fs[node].crash();
                    self.up[node] = true;
                    self.nodes[node] = (self.factories[node])();
                    self.callback(node, |n, env| n.on_start(env))?;
                }
            }
            self.check(invariants)?;
        }
        self.now_ns = self.now_ns.max(until_ns);
        Ok(())
    }

    fn check(&mut self, invariants: &mut [Invariant<'_, M>]) -> Result<(), Violation> {
        let ctx = InvariantCtx {
            now: SimTime(self.now_ns),
            fs: &self.fs,
            queued: &self.heap,
        };
        for inv in invariants.iter_mut() {
            if let Err(message) = inv(&ctx) {
                return Err(Violation {
                    message,
                    at_ns: self.now_ns,
                    steps: self.stats.steps,
                });
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> &RunStats {
        &self.stats
    }

    /// Durable image of one node's storage (for end-state assertions).
    pub fn durable(&self, node: NodeId, key: &str) -> Option<&[u8]> {
        self.fs[node].durable(key)
    }

    /// A stable fingerprint of the run for determinism tests: an FNV-style
    /// digest of every node's storage (BTreeMap iteration is ordered, so the
    /// Debug dump is stable) mixed with the run statistics.
    pub fn fingerprint(&self) -> u64 {
        let mut acc: u64 = 0xCBF2_9CE4_8422_2325;
        for fs in &self.fs {
            for b in format!("{fs:?}").bytes() {
                acc = (acc ^ u64::from(b)).wrapping_mul(0x100_0000_01B3);
            }
        }
        acc ^ self.stats.steps
            ^ self.stats.delivered.rotate_left(16)
            ^ self.stats.dropped.rotate_left(32)
            ^ self.stats.duplicated.rotate_left(48)
    }
}
