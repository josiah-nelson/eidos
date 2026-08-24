//! Materialize-at-ship replication state machines.
//!
//! The source owns a monotonic `(source, epoch, seq)` outbox. A shipped batch
//! covers a contiguous sequence interval but contains only the final row image
//! for each object touched inside that interval. The central applies those
//! images and advances its watermark in one atomic durable write, then ACKs.

use crate::env::{Env, Node, NodeId};
use crate::identity::{AdmissionState, BatchDecision, HelloDecision, SourceEpoch};
use crate::merkle::{MerkleTree, RecordDigest, MAX_FLEET_LEAF_BITS, MIN_FLEET_LEAF_BITS};
use crate::sim::InvariantCtx;
use eidos_domain::{ObjectId, SourceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const SHIPPER_STATE_KEY: &str = "sync/shipper";
pub const REPLICA_STATE_KEY: &str = "sync/replica";
pub const CURSOR_STATE_KEY: &str = "sync/cursors";

const TICK: u32 = 1;
pub const DEFAULT_TICK_NS: u64 = 20_000_000;
const MAX_ADMISSION_ALARMS: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalMutation {
    pub object: ObjectId,
    pub generation: u64,
    /// `None` is a generation-bearing tombstone.
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedChange {
    pub seq: u64,
    pub object: ObjectId,
    pub generation: u64,
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedBatch {
    pub source: SourceId,
    pub epoch: SourceEpoch,
    /// Central watermark from which this interval was materialized.
    pub after_seq: u64,
    /// Every source event through this sequence is represented by the final
    /// row images in `changes`, even when intermediate images were coalesced.
    pub through_seq: u64,
    pub changes: Vec<MaterializedChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub source: SourceId,
    pub epoch: SourceEpoch,
    pub through_seq: u64,
    pub rows: Vec<MaterializedChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOffer {
    pub source: SourceId,
    pub epoch: SourceEpoch,
    pub through_seq: u64,
    pub leaf_bits: u8,
    pub leaf_hashes: Vec<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRequest {
    pub source: SourceId,
    pub epoch: SourceEpoch,
    pub through_seq: u64,
    pub leaf_bits: u8,
    pub leaves: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRows {
    pub request: RepairRequest,
    pub rows: Vec<MaterializedChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMsg {
    Hello {
        source: SourceId,
        epoch: SourceEpoch,
        head_seq: u64,
        compacted_through: u64,
    },
    Resume {
        source: SourceId,
        epoch: SourceEpoch,
        after_seq: u64,
        requires_repair: bool,
    },
    FullResync {
        source: SourceId,
        epoch: SourceEpoch,
    },
    Batch(MaterializedBatch),
    Snapshot(SourceSnapshot),
    RepairOffer(RepairOffer),
    RepairRequest(RepairRequest),
    RepairRows(RepairRows),
    Ack {
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
    },
    Rejected {
        source: SourceId,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedRow {
    pub seq: u64,
    pub generation: u64,
    pub value: Option<Vec<u8>>,
}

/// Durable source log plus its current materialized source image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbox {
    pub next_seq: u64,
    pub compacted_through: u64,
    consumers: BTreeMap<NodeId, u64>,
    changes: Vec<MaterializedChange>,
    rows: BTreeMap<ObjectId, MaterializedRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OutboxError {
    #[error("source sequence space is exhausted")]
    SequenceExhausted,
    #[error("object {object} generation regressed from {current} to {offered}")]
    GenerationRewind {
        object: ObjectId,
        current: u64,
        offered: u64,
    },
    #[error("consumer {consumer} acknowledged {offered} beyond source head {head}")]
    AckBeyondHead {
        consumer: NodeId,
        offered: u64,
        head: u64,
    },
}

impl Outbox {
    pub fn new(consumers: impl IntoIterator<Item = NodeId>) -> Self {
        Self {
            next_seq: 0,
            compacted_through: 0,
            consumers: consumers.into_iter().map(|node| (node, 0)).collect(),
            changes: Vec::new(),
            rows: BTreeMap::new(),
        }
    }

    pub fn append(&mut self, mutation: LocalMutation) -> Result<u64, OutboxError> {
        if let Some(current) = self.rows.get(&mutation.object) {
            if mutation.generation < current.generation {
                return Err(OutboxError::GenerationRewind {
                    object: mutation.object,
                    current: current.generation,
                    offered: mutation.generation,
                });
            }
        }
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .ok_or(OutboxError::SequenceExhausted)?;
        let change = MaterializedChange {
            seq: self.next_seq,
            object: mutation.object,
            generation: mutation.generation,
            value: mutation.value,
        };
        self.rows.insert(
            change.object,
            MaterializedRow {
                seq: change.seq,
                generation: change.generation,
                value: change.value.clone(),
            },
        );
        self.changes.push(change);
        self.coalesce_unacknowledged();
        Ok(self.next_seq)
    }

    pub fn watermark(&self, consumer: NodeId) -> u64 {
        self.consumers.get(&consumer).copied().unwrap_or(0)
    }

    pub fn oldest_watermark(&self) -> u64 {
        self.consumers.values().copied().min().unwrap_or(0)
    }

    pub fn acknowledge(&mut self, consumer: NodeId, through_seq: u64) -> Result<(), OutboxError> {
        if through_seq > self.next_seq {
            return Err(OutboxError::AckBeyondHead {
                consumer,
                offered: through_seq,
                head: self.next_seq,
            });
        }
        let watermark = self.consumers.entry(consumer).or_default();
        *watermark = (*watermark).max(through_seq);
        self.compact();
        Ok(())
    }

    fn compact(&mut self) {
        let floor = self.oldest_watermark();
        self.changes.retain(|change| change.seq > floor);
        self.coalesce_unacknowledged();
        // A tombstone remains materialized until every registered consumer
        // has crossed it. Live rows remain for snapshots indefinitely.
        self.rows
            .retain(|_, row| row.value.is_some() || row.seq > floor);
        self.compacted_through = self.compacted_through.max(floor);
    }

    fn coalesce_unacknowledged(&mut self) {
        let mut latest = BTreeMap::<ObjectId, MaterializedChange>::new();
        for change in self.changes.drain(..) {
            latest.insert(change.object, change);
        }
        self.changes = latest.into_values().collect();
        self.changes.sort_by_key(|change| change.seq);
    }

    pub fn batch_after(
        &self,
        source: SourceId,
        epoch: SourceEpoch,
        after_seq: u64,
    ) -> Option<MaterializedBatch> {
        if after_seq < self.compacted_through {
            return None;
        }
        let selected: Vec<&MaterializedChange> = self
            .changes
            .iter()
            .filter(|change| change.seq > after_seq)
            .collect();
        if selected.is_empty() {
            return None;
        }
        let changes = selected.into_iter().cloned().collect();
        Some(MaterializedBatch {
            source,
            epoch,
            after_seq,
            through_seq: self.next_seq,
            changes,
        })
    }

    pub fn snapshot(&self, source: SourceId, epoch: SourceEpoch) -> SourceSnapshot {
        SourceSnapshot {
            source,
            epoch,
            through_seq: self.next_seq,
            rows: self
                .rows
                .iter()
                .map(|(object, row)| MaterializedChange {
                    seq: row.seq,
                    object: *object,
                    generation: row.generation,
                    value: row.value.clone(),
                })
                .collect(),
        }
    }

    pub fn rows(&self) -> &BTreeMap<ObjectId, MaterializedRow> {
        &self.rows
    }

    pub fn changes(&self) -> &[MaterializedChange] {
        &self.changes
    }

    pub fn validate(&self) -> Result<(), String> {
        let oldest = self.oldest_watermark();
        if self.compacted_through > oldest {
            return Err(format!(
                "outbox compacted through {} above oldest acknowledgement {oldest}",
                self.compacted_through
            ));
        }
        if self
            .changes
            .iter()
            .any(|change| change.seq <= self.compacted_through || change.seq > self.next_seq)
        {
            return Err("outbox contains a change outside its retained interval".into());
        }
        if self
            .rows
            .values()
            .any(|row| row.value.is_none() && row.seq <= oldest)
        {
            return Err("a tombstone survived below the compaction floor".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipperState {
    pub source: SourceId,
    pub epoch: SourceEpoch,
    pub outbox: Outbox,
}

pub struct Shipper {
    central: NodeId,
    tick_ns: u64,
    repair_leaf_bits: u8,
    script: Vec<LocalMutation>,
    initial: ShipperState,
    state: ShipperState,
}

impl Shipper {
    pub fn new(
        source: SourceId,
        epoch: SourceEpoch,
        central: NodeId,
        script: Vec<LocalMutation>,
    ) -> Self {
        let initial = ShipperState {
            source,
            epoch,
            outbox: Outbox::new([central]),
        };
        Self {
            central,
            tick_ns: DEFAULT_TICK_NS,
            repair_leaf_bits: MIN_FLEET_LEAF_BITS,
            script,
            state: initial.clone(),
            initial,
        }
    }

    pub fn with_tick_ns(mut self, tick_ns: u64) -> Self {
        self.tick_ns = tick_ns.max(1);
        self
    }

    pub fn from_state(state: ShipperState, central: NodeId) -> Self {
        Self {
            central,
            tick_ns: DEFAULT_TICK_NS,
            repair_leaf_bits: MIN_FLEET_LEAF_BITS,
            script: Vec::new(),
            initial: state.clone(),
            state,
        }
    }

    /// Tests can use a compact tree; the production default is 2^17 leaves.
    pub fn with_repair_leaf_bits(mut self, leaf_bits: u8) -> Self {
        assert!(leaf_bits <= MAX_FLEET_LEAF_BITS);
        self.repair_leaf_bits = leaf_bits;
        self
    }

    fn merkle_tree(&self) -> MerkleTree {
        MerkleTree::with_leaf_bits(
            self.repair_leaf_bits,
            self.state.outbox.rows().iter().map(|(object, row)| {
                RecordDigest::from_value(*object, row.generation, row.value.as_deref())
            }),
        )
    }

    fn send_repair(&self, env: &mut dyn Env<SyncMsg>) {
        let tree = self.merkle_tree();
        env.send(
            self.central,
            SyncMsg::RepairOffer(RepairOffer {
                source: self.state.source,
                epoch: self.state.epoch,
                through_seq: self.state.outbox.next_seq,
                leaf_bits: tree.leaf_bits(),
                leaf_hashes: tree.leaf_hashes(),
            }),
        );
    }

    fn persist(&self, env: &mut dyn Env<SyncMsg>) {
        env.fs().write_durable(
            SHIPPER_STATE_KEY,
            serde_json::to_vec(&self.state).expect("serialize shipper state"),
        );
    }

    fn send_batch(&self, env: &mut dyn Env<SyncMsg>) {
        let after = self.state.outbox.watermark(self.central);
        if after < self.state.outbox.compacted_through {
            self.send_repair(env);
        } else if let Some(batch) =
            self.state
                .outbox
                .batch_after(self.state.source, self.state.epoch, after)
        {
            env.send(self.central, SyncMsg::Batch(batch));
        }
    }

    fn send_progress(&self, env: &mut dyn Env<SyncMsg>) {
        env.send(
            self.central,
            SyncMsg::Hello {
                source: self.state.source,
                epoch: self.state.epoch,
                head_seq: self.state.outbox.next_seq,
                compacted_through: self.state.outbox.compacted_through,
            },
        );
        self.send_batch(env);
    }
}

impl Node for Shipper {
    type Msg = SyncMsg;

    fn on_start(&mut self, env: &mut dyn Env<SyncMsg>) {
        self.state = env
            .fs()
            .read(SHIPPER_STATE_KEY)
            .map(|bytes| serde_json::from_slice(bytes).expect("recover shipper state"))
            .unwrap_or_else(|| self.initial.clone());
        self.persist(env);
        self.send_progress(env);
        env.set_timer(self.tick_ns, TICK);
    }

    fn on_message(&mut self, env: &mut dyn Env<SyncMsg>, from: NodeId, msg: SyncMsg) {
        if from != self.central {
            return;
        }
        match msg {
            SyncMsg::Resume {
                source,
                epoch,
                after_seq,
                requires_repair,
            } if source == self.state.source && epoch == self.state.epoch => {
                if requires_repair {
                    self.send_repair(env);
                } else if after_seq >= self.state.outbox.watermark(self.central) {
                    if self
                        .state
                        .outbox
                        .acknowledge(self.central, after_seq)
                        .is_ok()
                    {
                        self.persist(env);
                    }
                    self.send_batch(env);
                }
            }
            SyncMsg::Ack {
                source,
                epoch,
                through_seq: after_seq,
            } if source == self.state.source && epoch == self.state.epoch => {
                if self
                    .state
                    .outbox
                    .acknowledge(self.central, after_seq)
                    .is_ok()
                {
                    self.persist(env);
                }
                self.send_batch(env);
            }
            SyncMsg::FullResync { source, epoch }
                if source == self.state.source && epoch == self.state.epoch =>
            {
                env.send(
                    self.central,
                    SyncMsg::Snapshot(self.state.outbox.snapshot(source, epoch)),
                );
            }
            SyncMsg::RepairRequest(request)
                if request.source == self.state.source && request.epoch == self.state.epoch =>
            {
                let tree = self.merkle_tree();
                if request.leaf_bits > MAX_FLEET_LEAF_BITS
                    || tree.leaf_bits() != request.leaf_bits
                    || request
                        .leaves
                        .iter()
                        .any(|leaf| *leaf >= (1u32 << request.leaf_bits))
                {
                    return;
                }
                let leaves: std::collections::BTreeSet<_> =
                    request.leaves.iter().copied().collect();
                let rows = self
                    .state
                    .outbox
                    .rows()
                    .iter()
                    .filter(|(object, _)| leaves.contains(&tree.leaf_for_object(**object)))
                    .map(|(object, row)| MaterializedChange {
                        seq: row.seq,
                        object: *object,
                        generation: row.generation,
                        value: row.value.clone(),
                    })
                    .collect();
                env.send(
                    self.central,
                    SyncMsg::RepairRows(RepairRows { request, rows }),
                );
            }
            _ => {}
        }
    }

    fn on_timer(&mut self, env: &mut dyn Env<SyncMsg>, timer: u32) {
        debug_assert_eq!(timer, TICK);
        let script_index = self.state.outbox.next_seq as usize;
        if let Some(mutation) = self.script.get(script_index).cloned() {
            self.state
                .outbox
                .append(mutation)
                .expect("script generations are monotonic");
            self.persist(env);
        }
        self.send_progress(env);
        env.set_timer(self.tick_ns, TICK);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicaState {
    pub sources: BTreeMap<SourceId, BTreeMap<ObjectId, MaterializedRow>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdmissionAlarm {
    SequenceRewind {
        source: SourceId,
        applied_seq: u64,
        offered_seq: u64,
    },
    RetiredEpoch {
        source: SourceId,
        current_epoch: SourceEpoch,
        offered_epoch: SourceEpoch,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CursorState {
    pub sources: BTreeMap<SourceId, AdmissionState>,
    /// Distinct admission failures, retaining the newest bounded window.
    pub alarms: Vec<AdmissionAlarm>,
}

pub struct Applier {
    replicas: ReplicaState,
    cursors: CursorState,
    /// Deliberate harness bug: false acknowledges buffered, non-durable
    /// effects so a crash can expose the lost apply.
    pub durable_before_ack: bool,
}

impl Applier {
    pub fn new(durable_before_ack: bool) -> Self {
        Self {
            replicas: ReplicaState::default(),
            cursors: CursorState::default(),
            durable_before_ack,
        }
    }

    fn persist_cursors(&self, env: &mut dyn Env<SyncMsg>) {
        env.fs().write_durable(
            CURSOR_STATE_KEY,
            serde_json::to_vec(&self.cursors).expect("serialize cursors"),
        );
    }

    fn commit(&self, env: &mut dyn Env<SyncMsg>, replicas: &ReplicaState, cursors: &CursorState) {
        let replica_bytes = serde_json::to_vec(replicas).expect("serialize replicas");
        let cursor_bytes = serde_json::to_vec(cursors).expect("serialize cursors");
        if self.durable_before_ack {
            env.fs().write_atomic(vec![
                (REPLICA_STATE_KEY.into(), replica_bytes),
                (CURSOR_STATE_KEY.into(), cursor_bytes),
            ]);
        } else {
            env.fs().write(REPLICA_STATE_KEY, replica_bytes);
            env.fs().write(CURSOR_STATE_KEY, cursor_bytes);
        }
    }

    fn reject(&self, env: &mut dyn Env<SyncMsg>, to: NodeId, source: SourceId, reason: String) {
        env.send(to, SyncMsg::Rejected { source, reason });
    }

    fn record_alarm(&mut self, env: &mut dyn Env<SyncMsg>, alarm: AdmissionAlarm) {
        if self.cursors.alarms.contains(&alarm) {
            return;
        }
        if self.cursors.alarms.len() >= MAX_ADMISSION_ALARMS {
            let stale = self.cursors.alarms.len() - MAX_ADMISSION_ALARMS + 1;
            self.cursors.alarms.drain(..stale);
        }
        self.cursors.alarms.push(alarm);
        self.persist_cursors(env);
    }

    fn on_hello(
        &mut self,
        env: &mut dyn Env<SyncMsg>,
        from: NodeId,
        source: SourceId,
        epoch: SourceEpoch,
        head_seq: u64,
        compacted_through: u64,
    ) {
        let admission = self
            .cursors
            .sources
            .entry(source)
            .or_insert_with(|| AdmissionState::new(source, epoch));
        let decision = admission.admit_hello(epoch, head_seq);
        match decision {
            HelloDecision::Incremental { after_seq } => {
                self.persist_cursors(env);
                env.send(
                    from,
                    SyncMsg::Resume {
                        source,
                        epoch,
                        after_seq,
                        requires_repair: after_seq < compacted_through,
                    },
                );
            }
            HelloDecision::FullResync { .. } => {
                self.persist_cursors(env);
                env.send(from, SyncMsg::FullResync { source, epoch });
            }
            HelloDecision::RejectAndAlarm {
                applied_seq,
                offered_seq,
            } => {
                self.record_alarm(
                    env,
                    AdmissionAlarm::SequenceRewind {
                        source,
                        applied_seq,
                        offered_seq,
                    },
                );
                self.reject(
                    env,
                    from,
                    source,
                    format!(
                        "same-epoch sequence rewind: central={applied_seq}, source={offered_seq}"
                    ),
                );
            }
            HelloDecision::RejectEpochAndAlarm {
                current_epoch,
                offered_epoch,
            } => {
                self.record_alarm(
                    env,
                    AdmissionAlarm::RetiredEpoch {
                        source,
                        current_epoch,
                        offered_epoch,
                    },
                );
                self.reject(
                    env,
                    from,
                    source,
                    format!(
                        "retired/conflicting epoch {offered_epoch}; active epoch is {current_epoch}"
                    ),
                );
            }
        }
    }

    fn valid_batch(batch: &MaterializedBatch) -> bool {
        let unique_objects = batch
            .changes
            .iter()
            .map(|change| change.object)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == batch.changes.len();
        batch.after_seq < batch.through_seq
            && unique_objects
            && batch
                .changes
                .iter()
                .all(|change| change.seq > batch.after_seq && change.seq <= batch.through_seq)
    }

    fn on_batch(&mut self, env: &mut dyn Env<SyncMsg>, from: NodeId, batch: MaterializedBatch) {
        if !Self::valid_batch(&batch) {
            self.reject(env, from, batch.source, "malformed batch interval".into());
            return;
        }
        let Some(admission) = self.cursors.sources.get(&batch.source) else {
            self.reject(
                env,
                from,
                batch.source,
                "hello required before batch".into(),
            );
            return;
        };
        match admission.admit_batch(batch.epoch, batch.after_seq, batch.through_seq) {
            BatchDecision::AlreadyApplied => {
                env.send(
                    from,
                    SyncMsg::Ack {
                        source: batch.source,
                        epoch: admission.epoch,
                        through_seq: admission.applied_seq,
                    },
                );
            }
            BatchDecision::Gap {
                expected_at_most,
                received,
            } => self.reject(
                env,
                from,
                batch.source,
                format!("sequence gap: central={expected_at_most}, batch-after={received}"),
            ),
            BatchDecision::FullResyncRequired => env.send(
                from,
                SyncMsg::FullResync {
                    source: batch.source,
                    epoch: batch.epoch,
                },
            ),
            BatchDecision::Apply => {
                let mut replicas = self.replicas.clone();
                let mut cursors = self.cursors.clone();
                let rows = replicas.sources.entry(batch.source).or_default();
                for change in batch.changes {
                    let current_seq = rows.get(&change.object).map(|row| row.seq).unwrap_or(0);
                    if change.seq > current_seq {
                        rows.insert(
                            change.object,
                            MaterializedRow {
                                seq: change.seq,
                                generation: change.generation,
                                value: change.value,
                            },
                        );
                    }
                }
                cursors
                    .sources
                    .get_mut(&batch.source)
                    .expect("admission existed")
                    .applied(batch.through_seq);
                self.commit(env, &replicas, &cursors);
                self.replicas = replicas;
                self.cursors = cursors;
                env.send(
                    from,
                    SyncMsg::Ack {
                        source: batch.source,
                        epoch: batch.epoch,
                        through_seq: batch.through_seq,
                    },
                );
            }
        }
    }

    fn on_snapshot(&mut self, env: &mut dyn Env<SyncMsg>, from: NodeId, snapshot: SourceSnapshot) {
        let unique_objects = snapshot
            .rows
            .iter()
            .map(|row| row.object)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == snapshot.rows.len();
        if !unique_objects
            || snapshot
                .rows
                .iter()
                .any(|row| row.seq > snapshot.through_seq)
        {
            self.reject(env, from, snapshot.source, "malformed snapshot".into());
            return;
        }
        let mut cursors = self.cursors.clone();
        let Some(admission) = cursors.sources.get_mut(&snapshot.source) else {
            self.reject(
                env,
                from,
                snapshot.source,
                "hello required before snapshot".into(),
            );
            return;
        };
        if !admission.snapshot_applied(snapshot.epoch, snapshot.through_seq) {
            self.reject(
                env,
                from,
                snapshot.source,
                "snapshot was not requested for this epoch".into(),
            );
            return;
        }
        let rows = snapshot
            .rows
            .into_iter()
            .map(|change| {
                (
                    change.object,
                    MaterializedRow {
                        seq: change.seq,
                        generation: change.generation,
                        value: change.value,
                    },
                )
            })
            .collect();
        let mut replicas = self.replicas.clone();
        replicas.sources.insert(snapshot.source, rows);
        self.commit(env, &replicas, &cursors);
        self.replicas = replicas;
        self.cursors = cursors;
        env.send(
            from,
            SyncMsg::Ack {
                source: snapshot.source,
                epoch: snapshot.epoch,
                through_seq: snapshot.through_seq,
            },
        );
    }

    fn replica_tree(&self, source: SourceId, leaf_bits: u8) -> MerkleTree {
        MerkleTree::with_leaf_bits(
            leaf_bits,
            self.replicas
                .sources
                .get(&source)
                .into_iter()
                .flat_map(|rows| rows.iter())
                .map(|(object, row)| {
                    RecordDigest::from_value(*object, row.generation, row.value.as_deref())
                }),
        )
    }

    fn on_repair_offer(&self, env: &mut dyn Env<SyncMsg>, from: NodeId, offer: RepairOffer) {
        let Some(admission) = self.cursors.sources.get(&offer.source) else {
            self.reject(
                env,
                from,
                offer.source,
                "hello required before repair".into(),
            );
            return;
        };
        if admission.epoch != offer.epoch || offer.through_seq < admission.applied_seq {
            self.reject(env, from, offer.source, "stale repair offer".into());
            return;
        }
        if offer.leaf_bits > MAX_FLEET_LEAF_BITS {
            self.reject(env, from, offer.source, "invalid Merkle shape".into());
            return;
        }
        let tree = self.replica_tree(offer.source, offer.leaf_bits);
        let local_hashes = tree.leaf_hashes();
        if offer.leaf_hashes.len() != local_hashes.len() {
            self.reject(
                env,
                from,
                offer.source,
                "invalid Merkle leaf manifest".into(),
            );
            return;
        }
        let leaves = local_hashes
            .iter()
            .zip(&offer.leaf_hashes)
            .enumerate()
            .filter_map(|(leaf, (local, remote))| (local != remote).then_some(leaf as u32))
            .collect();
        env.send(
            from,
            SyncMsg::RepairRequest(RepairRequest {
                source: offer.source,
                epoch: offer.epoch,
                through_seq: offer.through_seq,
                leaf_bits: offer.leaf_bits,
                leaves,
            }),
        );
    }

    fn on_repair_rows(&mut self, env: &mut dyn Env<SyncMsg>, from: NodeId, repair: RepairRows) {
        let request = repair.request;
        if request.leaf_bits > MAX_FLEET_LEAF_BITS {
            self.reject(env, from, request.source, "invalid Merkle shape".into());
            return;
        }
        if request
            .leaves
            .iter()
            .any(|leaf| *leaf >= (1u32 << request.leaf_bits))
            || repair.rows.iter().any(|row| row.seq > request.through_seq)
        {
            self.reject(
                env,
                from,
                request.source,
                "malformed repair response".into(),
            );
            return;
        }
        let Some(admission) = self.cursors.sources.get(&request.source) else {
            self.reject(
                env,
                from,
                request.source,
                "hello required before repair".into(),
            );
            return;
        };
        if admission.epoch != request.epoch || request.through_seq < admission.applied_seq {
            self.reject(env, from, request.source, "stale repair rows".into());
            return;
        }
        let local_tree = self.replica_tree(request.source, request.leaf_bits);
        let leaves: std::collections::BTreeSet<_> = request.leaves.iter().copied().collect();
        if repair
            .rows
            .iter()
            .any(|row| !leaves.contains(&local_tree.leaf_for_object(row.object)))
        {
            self.reject(
                env,
                from,
                request.source,
                "repair row outside requested leaf".into(),
            );
            return;
        }
        let mut replicas = self.replicas.clone();
        let rows = replicas.sources.entry(request.source).or_default();
        rows.retain(|object, _| !leaves.contains(&local_tree.leaf_for_object(*object)));
        for row in repair.rows {
            rows.insert(
                row.object,
                MaterializedRow {
                    seq: row.seq,
                    generation: row.generation,
                    value: row.value,
                },
            );
        }
        let mut cursors = self.cursors.clone();
        cursors
            .sources
            .get_mut(&request.source)
            .expect("admission existed")
            .applied(request.through_seq);
        self.commit(env, &replicas, &cursors);
        self.replicas = replicas;
        self.cursors = cursors;
        env.send(
            from,
            SyncMsg::Ack {
                source: request.source,
                epoch: request.epoch,
                through_seq: request.through_seq,
            },
        );
    }
}

impl Node for Applier {
    type Msg = SyncMsg;

    fn on_start(&mut self, env: &mut dyn Env<SyncMsg>) {
        self.replicas = env
            .fs()
            .read(REPLICA_STATE_KEY)
            .map(|bytes| serde_json::from_slice(bytes).expect("recover replicas"))
            .unwrap_or_default();
        self.cursors = env
            .fs()
            .read(CURSOR_STATE_KEY)
            .map(|bytes| serde_json::from_slice(bytes).expect("recover cursors"))
            .unwrap_or_default();
    }

    fn on_message(&mut self, env: &mut dyn Env<SyncMsg>, from: NodeId, msg: SyncMsg) {
        match msg {
            SyncMsg::Hello {
                source,
                epoch,
                head_seq,
                compacted_through,
            } => self.on_hello(env, from, source, epoch, head_seq, compacted_through),
            SyncMsg::Batch(batch) => self.on_batch(env, from, batch),
            SyncMsg::Snapshot(snapshot) => self.on_snapshot(env, from, snapshot),
            SyncMsg::RepairOffer(offer) => self.on_repair_offer(env, from, offer),
            SyncMsg::RepairRows(rows) => self.on_repair_rows(env, from, rows),
            _ => {}
        }
    }

    fn on_timer(&mut self, _env: &mut dyn Env<SyncMsg>, _timer: u32) {}
}

fn decode<T>(ctx: &InvariantCtx<'_, SyncMsg>, node: NodeId, key: &str) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    ctx.buffered(node, key)
        .map(|bytes| serde_json::from_slice(bytes).expect("decode invariant state"))
}

/// Safety: source compaction never crosses the oldest registered consumer
/// watermark; tombstones therefore cannot disappear while any consumer is
/// offline behind them.
pub fn compaction_respects_oldest_watermark(
    source_node: NodeId,
) -> impl FnMut(&InvariantCtx<'_, SyncMsg>) -> Result<(), String> {
    move |ctx| {
        decode::<ShipperState>(ctx, source_node, SHIPPER_STATE_KEY)
            .map(|state| state.outbox.validate())
            .unwrap_or(Ok(()))
    }
}

/// Safety: a live central watermark never regresses, including across crash
/// recovery. This catches acknowledging before the effect+watermark commit.
pub fn watermark_monotonic(
    central_node: NodeId,
    source: SourceId,
) -> impl FnMut(&InvariantCtx<'_, SyncMsg>) -> Result<(), String> {
    let mut previous = 0;
    move |ctx| {
        let cursors: CursorState = decode(ctx, central_node, CURSOR_STATE_KEY).unwrap_or_default();
        let current = cursors
            .sources
            .get(&source)
            .map(|state| state.applied_seq)
            .unwrap_or(0);
        if current < previous {
            return Err(format!(
                "source {source} watermark regressed {previous} -> {current}"
            ));
        }
        previous = current;
        Ok(())
    }
}

/// Durable effect rows may never claim a source sequence beyond the durable
/// cursor committed with them.
pub fn effects_do_not_lead_watermarks(
    central_node: NodeId,
) -> impl FnMut(&InvariantCtx<'_, SyncMsg>) -> Result<(), String> {
    move |ctx| {
        let replicas: ReplicaState = ctx
            .durable(central_node, REPLICA_STATE_KEY)
            .map(|bytes| serde_json::from_slice(bytes).expect("decode durable replicas"))
            .unwrap_or_default();
        let cursors: CursorState = ctx
            .durable(central_node, CURSOR_STATE_KEY)
            .map(|bytes| serde_json::from_slice(bytes).expect("decode durable cursors"))
            .unwrap_or_default();
        for (source, rows) in replicas.sources {
            let watermark = cursors
                .sources
                .get(&source)
                .map(|state| state.applied_seq)
                .unwrap_or(0);
            if let Some(row) = rows.values().find(|row| row.seq > watermark) {
                return Err(format!(
                    "source {source} durable row seq {} leads watermark {watermark}",
                    row.seq
                ));
            }
        }
        Ok(())
    }
}

/// Safety oracle for terminal deletes in a generated workload. Once the
/// source has produced a tombstone, a copy must remain in the retained source
/// state, at the central, or in flight. Tests pass only deletes that are not
/// followed by a later upsert of the same object.
pub fn no_lost_tombstones(
    source_node: NodeId,
    central_node: NodeId,
    source: SourceId,
    tombstones: Vec<(u64, ObjectId)>,
) -> impl FnMut(&InvariantCtx<'_, SyncMsg>) -> Result<(), String> {
    move |ctx| {
        let Some(shipper) = decode::<ShipperState>(ctx, source_node, SHIPPER_STATE_KEY) else {
            return Ok(());
        };
        let replicas: ReplicaState =
            decode(ctx, central_node, REPLICA_STATE_KEY).unwrap_or_default();
        for (seq, object) in &tombstones {
            if shipper.outbox.next_seq < *seq {
                continue;
            }
            let retained = shipper.outbox.changes().iter().any(|change| {
                change.object == *object && change.seq >= *seq && change.value.is_none()
            }) || shipper
                .outbox
                .rows()
                .get(object)
                .is_some_and(|row| row.seq >= *seq && row.value.is_none());
            let applied = replicas
                .sources
                .get(&source)
                .and_then(|rows| rows.get(object))
                .is_some_and(|row| row.seq >= *seq && row.value.is_none());
            let in_flight = ctx.in_flight().any(|(_, _, message)| match message {
                SyncMsg::Batch(batch) => batch.changes.iter().any(|change| {
                    change.object == *object && change.seq >= *seq && change.value.is_none()
                }),
                SyncMsg::Snapshot(snapshot) => snapshot.rows.iter().any(|change| {
                    change.object == *object && change.seq >= *seq && change.value.is_none()
                }),
                SyncMsg::RepairRows(repair) => repair.rows.iter().any(|change| {
                    change.object == *object && change.seq >= *seq && change.value.is_none()
                }),
                _ => false,
            });
            if !retained && !applied && !in_flight {
                return Err(format!(
                    "source {source} tombstone for {object} at seq {seq} was lost"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{Clock, Fs, SimFs, SimTime, Timers, Transport};

    #[derive(Default)]
    struct RecordingFs {
        inner: SimFs,
        fsyncs: usize,
    }

    impl Fs for RecordingFs {
        fn write(&mut self, key: &str, value: Vec<u8>) {
            self.inner.write(key, value);
        }

        fn fsync(&mut self, key: &str) {
            self.fsyncs += 1;
            self.inner.fsync(key);
        }

        fn read(&self, key: &str) -> Option<&[u8]> {
            self.inner.read(key)
        }

        fn write_atomic(&mut self, writes: Vec<(String, Vec<u8>)>) {
            self.inner.write_atomic(writes);
        }
    }

    #[derive(Default)]
    struct RecordingEnv {
        fs: RecordingFs,
        sent: Vec<(NodeId, SyncMsg)>,
    }

    impl Clock for RecordingEnv {
        fn now(&self) -> SimTime {
            SimTime(0)
        }
    }

    impl Transport<SyncMsg> for RecordingEnv {
        fn send(&mut self, to: NodeId, msg: SyncMsg) {
            self.sent.push((to, msg));
        }
    }

    impl Timers for RecordingEnv {
        fn set_timer(&mut self, _after_ns: u64, _timer: u32) {}
    }

    impl Env<SyncMsg> for RecordingEnv {
        fn fs(&mut self) -> &mut dyn Fs {
            &mut self.fs
        }
    }

    fn mutation(object: i64, generation: u64, value: Option<&[u8]>) -> LocalMutation {
        LocalMutation {
            object: ObjectId::new(object),
            generation,
            value: value.map(<[u8]>::to_vec),
        }
    }

    #[test]
    fn materialize_at_ship_coalesces_intermediate_images() {
        let mut outbox = Outbox::new([1]);
        outbox.append(mutation(1, 1, Some(b"a"))).unwrap();
        outbox.append(mutation(1, 2, Some(b"b"))).unwrap();
        outbox.append(mutation(2, 1, Some(b"c"))).unwrap();
        let batch = outbox
            .batch_after(SourceId::new(4), SourceEpoch::random_v4(1, 2), 0)
            .unwrap();
        assert_eq!(batch.through_seq, 3);
        assert_eq!(batch.changes.len(), 2);
        assert_eq!(batch.changes[0].seq, 2);
        assert_eq!(batch.changes[0].value.as_deref(), Some(&b"b"[..]));

        for generation in 3..=500 {
            outbox
                .append(mutation(1, generation, Some(b"latest")))
                .unwrap();
        }
        let batch = outbox
            .batch_after(SourceId::new(4), SourceEpoch::random_v4(1, 2), 0)
            .unwrap();
        assert_eq!(batch.through_seq, 501);
        assert_eq!(
            batch
                .changes
                .iter()
                .filter(|change| change.object == ObjectId::new(1))
                .count(),
            1,
            "500 offline edits to one object ship as one final image"
        );
    }

    #[test]
    fn tombstone_waits_for_the_oldest_consumer() {
        let mut outbox = Outbox::new([1, 2]);
        outbox.append(mutation(1, 1, Some(b"present"))).unwrap();
        outbox.append(mutation(1, 2, None)).unwrap();
        outbox.acknowledge(1, 2).unwrap();
        assert!(outbox
            .rows()
            .get(&ObjectId::new(1))
            .unwrap()
            .value
            .is_none());
        assert_eq!(outbox.compacted_through, 0);
        outbox.acknowledge(2, 1).unwrap();
        assert!(outbox.rows().contains_key(&ObjectId::new(1)));
        outbox.acknowledge(2, 2).unwrap();
        assert!(!outbox.rows().contains_key(&ObjectId::new(1)));
        assert_eq!(outbox.compacted_through, 2);
    }

    #[test]
    fn repeated_rejection_is_durable_once_and_alarm_history_is_bounded() {
        let source = SourceId::new(7);
        let epoch = SourceEpoch::random_v4(1, 2);
        let mut admission = AdmissionState::new(source, epoch);
        admission.applied((MAX_ADMISSION_ALARMS + 10) as u64);
        let mut applier = Applier::new(true);
        applier.cursors.sources.insert(source, admission);
        let mut env = RecordingEnv::default();

        for _ in 0..100 {
            applier.on_hello(&mut env, 3, source, epoch, 9, 0);
        }
        assert_eq!(applier.cursors.alarms.len(), 1);
        assert_eq!(env.fs.fsyncs, 1, "identical alarms must not rewrite state");
        assert_eq!(
            env.sent.len(),
            100,
            "every retry still receives a rejection"
        );

        for offered_seq in 0..(MAX_ADMISSION_ALARMS as u64 + 5) {
            applier.on_hello(&mut env, 3, source, epoch, offered_seq, 0);
        }
        assert_eq!(applier.cursors.alarms.len(), MAX_ADMISSION_ALARMS);
        assert!(matches!(
            applier.cursors.alarms.last(),
            Some(AdmissionAlarm::SequenceRewind {
                offered_seq,
                ..
            }) if *offered_seq == MAX_ADMISSION_ALARMS as u64 + 4
        ));
    }
}
