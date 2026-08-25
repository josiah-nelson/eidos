//! Generated, replayable source workloads and the ghost oracle they imply.
//!
//! A [`WorkloadPlan`] is the second half of a universe beside
//! [`crate::sim::FaultPlan`]: what the sources *do* while the network and
//! processes misbehave. It is versioned, serializable, shrinkable, and
//! validated, so a failing seed replays byte-for-byte and minimizes to the
//! smallest history that still fails.
//!
//! [`GhostHistory`] is the independent oracle. It replays a source's script
//! without the protocol and records the exact materialized image after every
//! sequence, for every incarnation (epoch) and every fork (a rewind restores
//! an older durable state and continues from it). The central's durable
//! replica must, at every instant, be the image of *one* history at its own
//! durable cursor: no row ahead of the cursor, no row from a different fork,
//! no live row for an object that history has deleted. Tombstones may be
//! absent, because retention releases them once every consumer has crossed
//! them, but never wrong.

use crate::identity::SourceEpoch;
use crate::protocol::{LocalMutation, MaterializedRow};
use crate::rng::DeterministicRng;
use eidos_domain::{ObjectId, SourceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const WORKLOAD_SCHEMA_VERSION: u32 = 1;

/// One step of a source's life. Steps run one per shipper tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SourceOp {
    /// Upsert (`value: Some`) or delete (`value: None`) with a generation
    /// that never regresses per object.
    Mutate(LocalMutation),
    /// Restore/clone/rebuild: a fresh incarnation with a new epoch whose
    /// history restarts from the current live rows.
    EpochChange,
    /// Remember the current durable state for a later [`SourceOp::Rewind`].
    Checkpoint,
    /// Operator restores the last checkpoint on disk and the source keeps
    /// running from it without a new epoch. This is the split history the
    /// central must fence.
    Rewind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceWorkload {
    pub source: SourceId,
    /// Deterministic epoch material: incarnation `n` uses
    /// `SourceEpoch::random_v4(epoch_seed, n)`.
    pub epoch_seed: u64,
    pub ops: Vec<SourceOp>,
}

impl SourceWorkload {
    pub fn epoch(&self, incarnation: u64) -> SourceEpoch {
        SourceEpoch::random_v4(self.epoch_seed, incarnation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadPlan {
    pub version: u32,
    pub sources: Vec<SourceWorkload>,
}

/// Generation knobs. Every count is an inclusive upper bound; the generator
/// draws the actual size so shrinking has room below it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadParams {
    pub max_sources: usize,
    pub max_objects: usize,
    pub max_ops: usize,
    /// Share of mutations that delete an existing object.
    pub delete_permille: u32,
    /// Share of mutations that recreate a deleted object.
    pub recreate_permille: u32,
    /// Share of mutations that pick one hot object rather than a uniform one.
    pub hot_permille: u32,
    pub epoch_change_permille: u32,
    pub checkpoint_permille: u32,
    pub rewind_permille: u32,
}

impl WorkloadParams {
    /// Small universes for the million-seed nightly gate: cheap to run, wide
    /// enough to reach every protocol transition.
    pub fn soak() -> Self {
        Self {
            max_sources: 2,
            max_objects: 5,
            max_ops: 14,
            delete_permille: 200,
            recreate_permille: 500,
            hot_permille: 400,
            epoch_change_permille: 60,
            checkpoint_permille: 120,
            rewind_permille: 80,
        }
    }

    /// Larger histories for the scenario suite.
    pub fn scenario() -> Self {
        Self {
            max_sources: 3,
            max_objects: 12,
            max_ops: 60,
            ..Self::soak()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkloadError {
    #[error("unsupported workload version {0}")]
    Version(u32),
    #[error("workload has no sources")]
    NoSources,
    #[error("source {0} appears twice")]
    DuplicateSource(SourceId),
    #[error("source {source_id} op {index}: generation of object {object} regressed {current} -> {offered}")]
    GenerationRewind {
        source_id: SourceId,
        index: usize,
        object: ObjectId,
        current: u64,
        offered: u64,
    },
    #[error("source {source_id} op {index}: rewind without a checkpoint")]
    RewindWithoutCheckpoint { source_id: SourceId, index: usize },
}

impl WorkloadPlan {
    pub fn random(rng: &mut DeterministicRng, params: &WorkloadParams) -> Self {
        let sources = 1 + rng.below(params.max_sources.max(1) as u64) as usize;
        let mut plan = Self {
            version: WORKLOAD_SCHEMA_VERSION,
            sources: Vec::with_capacity(sources),
        };
        for index in 0..sources {
            let source = SourceId::new(index as i64 + 1);
            let objects = 1 + rng.below(params.max_objects.max(1) as u64) as usize;
            let ops = rng.below(params.max_ops as u64 + 1) as usize;
            let hot = ObjectId::new(1 + rng.below(objects as u64) as i64);
            let mut generations: BTreeMap<ObjectId, u64> = BTreeMap::new();
            let mut live: BTreeMap<ObjectId, bool> = BTreeMap::new();
            let mut checkpointed = false;
            let mut script = Vec::with_capacity(ops);
            for _ in 0..ops {
                if rng.chance(params.epoch_change_permille) {
                    script.push(SourceOp::EpochChange);
                    // A fresh incarnation forgets tombstones; generations of
                    // live rows carry over unchanged.
                    live.retain(|_, alive| *alive);
                    checkpointed = false;
                    continue;
                }
                if checkpointed && rng.chance(params.rewind_permille) {
                    script.push(SourceOp::Rewind);
                    // Keep generating on the restored history: the generator
                    // cannot know it, so replay it from the ghost below.
                    let ghost = GhostHistory::replay(&SourceWorkload {
                        source,
                        epoch_seed: 0,
                        ops: script.clone(),
                    });
                    let image = ghost.current_image();
                    generations = image
                        .iter()
                        .map(|(object, row)| (*object, row.generation))
                        .collect();
                    live = image
                        .iter()
                        .map(|(object, row)| (*object, row.value.is_some()))
                        .collect();
                    continue;
                }
                if rng.chance(params.checkpoint_permille) {
                    script.push(SourceOp::Checkpoint);
                    checkpointed = true;
                    continue;
                }
                let object = if rng.chance(params.hot_permille) {
                    hot
                } else {
                    ObjectId::new(1 + rng.below(objects as u64) as i64)
                };
                let alive = live.get(&object).copied().unwrap_or(false);
                let known = live.contains_key(&object);
                let delete = alive && rng.chance(params.delete_permille);
                if known && !alive && !rng.chance(params.recreate_permille) {
                    // Leave the tombstone alone this step; touch the hot
                    // object instead so the op count stays meaningful.
                    let gen = generations.entry(hot).or_insert(0);
                    *gen += 1;
                    live.insert(hot, true);
                    script.push(SourceOp::Mutate(LocalMutation {
                        object: hot,
                        generation: *gen,
                        value: Some(format!("{hot}:{gen}").into_bytes()),
                    }));
                    continue;
                }
                let gen = generations.entry(object).or_insert(0);
                *gen += 1;
                live.insert(object, !delete);
                script.push(SourceOp::Mutate(LocalMutation {
                    object,
                    generation: *gen,
                    value: (!delete).then(|| format!("{object}:{gen}").into_bytes()),
                }));
            }
            plan.sources.push(SourceWorkload {
                source,
                epoch_seed: 0x574f_524b_0000 + index as u64,
                ops: script,
            });
        }
        plan
    }

    pub fn validate(&self) -> Result<(), WorkloadError> {
        if self.version != WORKLOAD_SCHEMA_VERSION {
            return Err(WorkloadError::Version(self.version));
        }
        if self.sources.is_empty() {
            return Err(WorkloadError::NoSources);
        }
        let mut seen = std::collections::BTreeSet::new();
        for workload in &self.sources {
            if !seen.insert(workload.source) {
                return Err(WorkloadError::DuplicateSource(workload.source));
            }
            let mut ghost = GhostHistory::default();
            for (index, op) in workload.ops.iter().enumerate() {
                match op {
                    SourceOp::Mutate(mutation) => {
                        let current = ghost
                            .current_image()
                            .get(&mutation.object)
                            .map(|row| row.generation)
                            .unwrap_or(0);
                        if mutation.generation < current {
                            return Err(WorkloadError::GenerationRewind {
                                source_id: workload.source,
                                index,
                                object: mutation.object,
                                current,
                                offered: mutation.generation,
                            });
                        }
                    }
                    SourceOp::Rewind if ghost.checkpoint.is_none() => {
                        return Err(WorkloadError::RewindWithoutCheckpoint {
                            source_id: workload.source,
                            index,
                        });
                    }
                    _ => {}
                }
                ghost.step(op);
            }
        }
        Ok(())
    }

    pub fn source(&self, source: SourceId) -> Option<&SourceWorkload> {
        self.sources.iter().find(|w| w.source == source)
    }

    /// Terminal deletes per source: `(seq, object)` pairs whose tombstone is
    /// never followed by a recreate within the same history fork. These are
    /// the tombstones a consumer must never lose.
    pub fn terminal_tombstones(&self, source: SourceId) -> Vec<(u64, ObjectId)> {
        let Some(workload) = self.source(source) else {
            return Vec::new();
        };
        let ghost = GhostHistory::replay(workload);
        ghost.terminal_tombstones()
    }
}

/// Materialized image at one sequence of one history.
pub type Image = BTreeMap<ObjectId, MaterializedRow>;

/// One linear history: an incarnation (epoch) after possible rewinds. Each
/// fork is its own history sharing a prefix with the one it forked from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct History {
    pub incarnation: u64,
    /// `images[k]` is the image after sequence `k`; `images[0]` is empty for
    /// a first incarnation and the carried-over live rows otherwise.
    pub images: Vec<Image>,
}

impl History {
    pub fn head(&self) -> u64 {
        self.images.len() as u64 - 1
    }

    pub fn image(&self, seq: u64) -> Option<&Image> {
        self.images.get(seq as usize)
    }
}

/// The oracle: every history a source has ever claimed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GhostHistory {
    pub histories: Vec<History>,
    /// Index of the history the source is currently extending.
    pub current: usize,
    checkpoint: Option<(usize, u64)>,
}

impl GhostHistory {
    pub fn replay(workload: &SourceWorkload) -> Self {
        let mut ghost = Self::default();
        for op in &workload.ops {
            ghost.step(op);
        }
        ghost
    }

    fn ensure_started(&mut self) {
        if self.histories.is_empty() {
            self.histories.push(History {
                incarnation: 0,
                images: vec![Image::new()],
            });
            self.current = 0;
        }
    }

    pub fn current_history(&self) -> Option<&History> {
        self.histories.get(self.current)
    }

    pub fn current_image(&self) -> Image {
        self.current_history()
            .and_then(|h| h.images.last().cloned())
            .unwrap_or_default()
    }

    pub fn step(&mut self, op: &SourceOp) {
        self.ensure_started();
        match op {
            SourceOp::Mutate(mutation) => {
                let history = &mut self.histories[self.current];
                let mut image = history.images.last().cloned().unwrap_or_default();
                let seq = history.images.len() as u64;
                image.insert(
                    mutation.object,
                    MaterializedRow {
                        seq,
                        generation: mutation.generation,
                        value: mutation.value.clone(),
                    },
                );
                history.images.push(image);
            }
            SourceOp::EpochChange => {
                let previous = &self.histories[self.current];
                let incarnation = previous.incarnation + 1;
                // A rebuilt incarnation renumbers its live rows from 1 in
                // object order and carries no tombstones.
                let mut image = Image::new();
                let mut seq = 0;
                for (object, row) in previous.images.last().into_iter().flatten() {
                    if row.value.is_some() {
                        seq += 1;
                        image.insert(
                            *object,
                            MaterializedRow {
                                seq,
                                generation: row.generation,
                                value: row.value.clone(),
                            },
                        );
                    }
                }
                // Snapshot rows are the incarnation's own sequence prefix:
                // image k contains the first k rows.
                let mut images = Vec::with_capacity(image.len() + 1);
                let mut partial = Image::new();
                images.push(partial.clone());
                for (object, row) in &image {
                    partial.insert(*object, row.clone());
                    images.push(partial.clone());
                }
                self.histories.push(History {
                    incarnation,
                    images,
                });
                self.current = self.histories.len() - 1;
                self.checkpoint = None;
            }
            SourceOp::Checkpoint => {
                let history = &self.histories[self.current];
                self.checkpoint = Some((self.current, history.head()));
            }
            SourceOp::Rewind => {
                let Some((history_index, seq)) = self.checkpoint else {
                    return;
                };
                let base = &self.histories[history_index];
                let forked = History {
                    incarnation: base.incarnation,
                    images: base.images[..=seq as usize].to_vec(),
                };
                self.histories.push(forked);
                self.current = self.histories.len() - 1;
            }
        }
    }

    /// Terminal tombstones of the *current* history: deletes never followed
    /// by a recreate in that history.
    pub fn terminal_tombstones(&self) -> Vec<(u64, ObjectId)> {
        let Some(history) = self.current_history() else {
            return Vec::new();
        };
        let Some(last) = history.images.last() else {
            return Vec::new();
        };
        last.iter()
            .filter(|(_, row)| row.value.is_none())
            .map(|(object, row)| (row.seq, *object))
            .collect()
    }

    /// Whether a durable replica image `replica` at cursor `applied_seq` is
    /// the image of some history at that sequence. Returns the offending
    /// detail on failure.
    pub fn check_replica(
        &self,
        epoch_of: impl Fn(u64) -> SourceEpoch,
        replica_epoch: SourceEpoch,
        applied_seq: u64,
        replica: &Image,
    ) -> Result<(), String> {
        if self.histories.is_empty() {
            return if replica.is_empty() && applied_seq == 0 {
                Ok(())
            } else {
                Err("replica has state for a source that never ran".into())
            };
        }
        let mut reasons = Vec::new();
        for (index, history) in self.histories.iter().enumerate() {
            if epoch_of(history.incarnation) != replica_epoch {
                continue;
            }
            let Some(expected) = history.image(applied_seq) else {
                reasons.push(format!(
                    "history {index}: cursor {applied_seq} beyond head {}",
                    history.head()
                ));
                continue;
            };
            match compare(expected, replica, applied_seq) {
                Ok(()) => return Ok(()),
                Err(reason) => reasons.push(format!("history {index}: {reason}")),
            }
        }
        if reasons.is_empty() {
            reasons.push(format!(
                "replica epoch {replica_epoch} matches no incarnation of this source"
            ));
        }
        Err(format!(
            "durable replica at cursor {applied_seq} is not the image of any single history: {}",
            reasons.join("; ")
        ))
    }
}

fn compare(expected: &Image, replica: &Image, applied_seq: u64) -> Result<(), String> {
    for (object, row) in replica {
        if row.seq > applied_seq {
            return Err(format!("object {object} row seq {} leads cursor", row.seq));
        }
        match expected.get(object) {
            None => return Err(format!("object {object} exists but history never wrote it")),
            Some(want) if want.value.is_none() => {
                if row.value.is_some() {
                    return Err(format!(
                        "object {object} is live in the replica but deleted in history"
                    ));
                }
                if row.generation != want.generation || row.seq != want.seq {
                    return Err(format!(
                        "object {object} tombstone is (seq {}, gen {}) but history has (seq {}, gen {})",
                        row.seq, row.generation, want.seq, want.generation
                    ));
                }
            }
            Some(want) => {
                if row != want {
                    return Err(format!(
                        "object {object} is {row:?} but history has {want:?}"
                    ));
                }
            }
        }
    }
    for (object, want) in expected {
        if want.value.is_some() && !replica.contains_key(object) {
            return Err(format!("object {object} is live in history but missing"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutate(object: i64, generation: u64, live: bool) -> SourceOp {
        SourceOp::Mutate(LocalMutation {
            object: ObjectId::new(object),
            generation,
            value: live.then(|| format!("{object}:{generation}").into_bytes()),
        })
    }

    fn workload(ops: Vec<SourceOp>) -> SourceWorkload {
        SourceWorkload {
            source: SourceId::new(1),
            epoch_seed: 7,
            ops,
        }
    }

    #[test]
    fn generated_plans_validate_and_replay_deterministically() {
        for seed in 0..2_000u64 {
            let a = WorkloadPlan::random(&mut DeterministicRng::new(seed), &WorkloadParams::soak());
            let b = WorkloadPlan::random(&mut DeterministicRng::new(seed), &WorkloadParams::soak());
            assert_eq!(a, b, "seed {seed} is not deterministic");
            a.validate().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            let json = serde_json::to_string(&a).unwrap();
            assert_eq!(serde_json::from_str::<WorkloadPlan>(&json).unwrap(), a);
        }
    }

    #[test]
    fn generator_reaches_every_op_kind() {
        let mut kinds = [false; 4];
        for seed in 0..500u64 {
            let plan =
                WorkloadPlan::random(&mut DeterministicRng::new(seed), &WorkloadParams::soak());
            for op in plan.sources.iter().flat_map(|s| s.ops.iter()) {
                let k = match op {
                    SourceOp::Mutate(m) if m.value.is_none() => 0,
                    SourceOp::Mutate(_) => 1,
                    SourceOp::EpochChange => 2,
                    SourceOp::Checkpoint | SourceOp::Rewind => 3,
                };
                kinds[k] = true;
            }
        }
        assert!(kinds.iter().all(|k| *k), "{kinds:?}");
    }

    #[test]
    fn ghost_tracks_incarnations_and_forks() {
        let w = workload(vec![
            mutate(1, 1, true),
            mutate(2, 1, true),
            SourceOp::Checkpoint,
            mutate(2, 2, false),
            SourceOp::Rewind,
            mutate(1, 2, true),
            SourceOp::EpochChange,
            mutate(3, 1, true),
        ]);
        let ghost = GhostHistory::replay(&w);
        assert_eq!(ghost.histories.len(), 3);
        // Original history: two rows then a delete.
        assert_eq!(ghost.histories[0].head(), 3);
        assert!(ghost.histories[0].images[3][&ObjectId::new(2)]
            .value
            .is_none());
        // Fork: restored after seq 2, then object 1 generation 2 at seq 3.
        let fork = &ghost.histories[1];
        assert_eq!(fork.incarnation, 0);
        assert_eq!(fork.head(), 3);
        assert_eq!(fork.images[3][&ObjectId::new(1)].generation, 2);
        assert!(fork.images[3][&ObjectId::new(2)].value.is_some());
        // New incarnation: two live rows renumbered, then object 3.
        let inc = &ghost.histories[2];
        assert_eq!(inc.incarnation, 1);
        assert_eq!(inc.images[2].len(), 2);
        assert_eq!(inc.head(), 3);
        assert_eq!(inc.images[3][&ObjectId::new(3)].seq, 3);
        assert_eq!(ghost.terminal_tombstones(), vec![]);
    }

    #[test]
    fn check_replica_accepts_prefixes_of_one_history_only() {
        let w = workload(vec![
            mutate(1, 1, true),
            SourceOp::Checkpoint,
            mutate(2, 1, true),
            SourceOp::Rewind,
            mutate(3, 1, true),
        ]);
        let ghost = GhostHistory::replay(&w);
        let epoch = |n| w.epoch(n);
        let h0 = &ghost.histories[0];
        let h1 = &ghost.histories[1];
        assert!(ghost
            .check_replica(epoch, w.epoch(0), 2, &h0.images[2])
            .is_ok());
        assert!(ghost
            .check_replica(epoch, w.epoch(0), 2, &h1.images[2])
            .is_ok());
        // Mixed fork: object 2 from history 0 and object 3 from history 1.
        let mut mixed = h0.images[2].clone();
        mixed.extend(h1.images[2].clone());
        assert!(ghost.check_replica(epoch, w.epoch(0), 2, &mixed).is_err());
        // Row ahead of cursor.
        assert!(ghost
            .check_replica(epoch, w.epoch(0), 1, &h0.images[2])
            .is_err());
        // Wrong epoch.
        assert!(ghost
            .check_replica(epoch, w.epoch(1), 2, &h0.images[2])
            .is_err());
        // Missing tombstone is fine; live row for a deleted object is not.
        let w2 = workload(vec![mutate(1, 1, true), mutate(1, 2, false)]);
        let g2 = GhostHistory::replay(&w2);
        assert!(g2
            .check_replica(|n| w2.epoch(n), w2.epoch(0), 2, &Image::new())
            .is_ok());
        assert!(g2
            .check_replica(|n| w2.epoch(n), w2.epoch(0), 2, &g2.histories[0].images[1])
            .is_err());
    }

    #[test]
    fn validation_rejects_regressions_and_orphan_rewinds() {
        let bad = WorkloadPlan {
            version: 1,
            sources: vec![workload(vec![mutate(1, 2, true), mutate(1, 1, true)])],
        };
        assert!(matches!(
            bad.validate(),
            Err(WorkloadError::GenerationRewind { .. })
        ));
        let orphan = WorkloadPlan {
            version: 1,
            sources: vec![workload(vec![SourceOp::Rewind])],
        };
        assert!(matches!(
            orphan.validate(),
            Err(WorkloadError::RewindWithoutCheckpoint { .. })
        ));
    }
}
