//! Stable source identity, source epochs, and central admission fencing.
//!
//! A source belongs to a volume/root, not to the host currently serving it.
//! Its epoch is a UUID-shaped random fencing token. Any event that can make
//! the local sequence rewind (restore, clone, rebuild, or USN journal-id
//! change) must install a fresh epoch before another delta is admitted.

use eidos_domain::SourceId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// A UUID v4-shaped source incarnation/fencing token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceEpoch([u8; 16]);

impl SourceEpoch {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn random_v4(high: u64, low: u64) -> Self {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&high.to_be_bytes());
        bytes[8..].copy_from_slice(&low.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for SourceEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
}

impl FromStr for SourceEpoch {
    type Err = EpochParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 36
            || value.as_bytes().get(8) != Some(&b'-')
            || value.as_bytes().get(13) != Some(&b'-')
            || value.as_bytes().get(18) != Some(&b'-')
            || value.as_bytes().get(23) != Some(&b'-')
        {
            return Err(EpochParseError(value.to_string()));
        }
        let compact: Vec<u8> = value.bytes().filter(|byte| *byte != b'-').collect();
        if compact.len() != 32 {
            return Err(EpochParseError(value.to_string()));
        }
        let mut bytes = [0u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let high =
                hex_nibble(compact[index * 2]).ok_or_else(|| EpochParseError(value.to_string()))?;
            let low = hex_nibble(compact[index * 2 + 1])
                .ok_or_else(|| EpochParseError(value.to_string()))?;
            *byte = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl Serialize for SourceEpoch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SourceEpoch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid source epoch UUID {0:?}")]
pub struct EpochParseError(String);

/// The stable binding of a configured source. Host attribution is
/// deliberately absent: moving the volume to another host must not mint a
/// new source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBinding {
    pub source_id: SourceId,
    pub volume_serial: u64,
    pub root_path: String,
}

impl SourceBinding {
    pub fn matches(&self, volume_serial: u64, root_path: &str) -> bool {
        self.volume_serial == volume_serial && self.root_path.eq_ignore_ascii_case(root_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIncarnation {
    pub binding: SourceBinding,
    pub epoch: SourceEpoch,
    pub journal_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalObservation {
    Unchanged,
    Initialized,
    EpochBumped {
        old_epoch: SourceEpoch,
        new_epoch: SourceEpoch,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentityError {
    #[error("a journal-id change requires a fresh source epoch")]
    ReusedEpoch,
}

impl SourceIncarnation {
    /// Record the current USN journal. A changed journal id is an explicit
    /// epoch transition and therefore forces a full resync at the central.
    pub fn observe_journal(
        &mut self,
        journal_id: u64,
        fresh_epoch: SourceEpoch,
    ) -> Result<JournalObservation, IdentityError> {
        match self.journal_id {
            None => {
                self.journal_id = Some(journal_id);
                Ok(JournalObservation::Initialized)
            }
            Some(current) if current == journal_id => Ok(JournalObservation::Unchanged),
            Some(_) if fresh_epoch == self.epoch => Err(IdentityError::ReusedEpoch),
            Some(_) => {
                let old_epoch = self.epoch;
                self.epoch = fresh_epoch;
                self.journal_id = Some(journal_id);
                Ok(JournalObservation::EpochBumped {
                    old_epoch,
                    new_epoch: fresh_epoch,
                })
            }
        }
    }
}

/// Durable central-side admission state for one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionState {
    pub source_id: SourceId,
    pub epoch: SourceEpoch,
    pub applied_seq: u64,
    /// History chain hash at `applied_seq`: every batch must resume from
    /// exactly this point with exactly this hash, so a source restored to an
    /// older state that then outruns the cursor is fenced rather than
    /// silently merged. Required on load: durable state recorded before
    /// chains existed cannot be trusted to resume and must not be
    /// reinterpreted as genesis.
    pub applied_chain: ChainHash,
    pending_epoch: Option<SourceEpoch>,
    #[serde(default)]
    retired_epochs: BTreeSet<SourceEpoch>,
}

/// Hash chain over a source incarnation's sequence: `chain(0)` is all
/// zeros and `chain(n) = blake3(chain(n-1) || object || generation ||
/// value-or-tombstone)`. Two histories that share a prefix share its hashes
/// and differ from the first divergent sequence on.
pub type ChainHash = [u8; 32];

pub const CHAIN_GENESIS: ChainHash = [0u8; 32];

pub fn chain_next(
    previous: &ChainHash,
    object: i64,
    generation: u64,
    value: Option<&[u8]>,
) -> ChainHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(previous);
    hasher.update(&object.to_le_bytes());
    hasher.update(&generation.to_le_bytes());
    match value {
        Some(bytes) => {
            hasher.update(&[1]);
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        None => {
            hasher.update(&[0]);
        }
    }
    *hasher.finalize().as_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloDecision {
    Incremental {
        after_seq: u64,
    },
    FullResync {
        previous_epoch: SourceEpoch,
        new_epoch: SourceEpoch,
    },
    RejectAndAlarm {
        applied_seq: u64,
        offered_seq: u64,
    },
    RejectEpochAndAlarm {
        current_epoch: SourceEpoch,
        offered_epoch: SourceEpoch,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDecision {
    Apply,
    AlreadyApplied,
    /// The batch was cut before the cursor; ask for an exact resume rather
    /// than merging an interval whose start we cannot verify.
    Stale {
        applied_seq: u64,
    },
    Gap {
        expected_at_most: u64,
        received: u64,
    },
    /// Same epoch, same cursor, different history: the source was rewound
    /// and rewritten past our watermark.
    HistoryFork {
        applied_seq: u64,
        expected: ChainHash,
        offered: ChainHash,
    },
    FullResyncRequired,
}

impl AdmissionState {
    pub fn new(source_id: SourceId, epoch: SourceEpoch) -> Self {
        Self {
            source_id,
            epoch,
            applied_seq: 0,
            applied_chain: CHAIN_GENESIS,
            pending_epoch: None,
            retired_epochs: BTreeSet::new(),
        }
    }

    pub fn admit_hello(&mut self, epoch: SourceEpoch, source_head: u64) -> HelloDecision {
        if self.retired_epochs.contains(&epoch) {
            return HelloDecision::RejectEpochAndAlarm {
                current_epoch: self.epoch,
                offered_epoch: epoch,
            };
        }
        if let Some(pending) = self.pending_epoch {
            if epoch == pending {
                return HelloDecision::FullResync {
                    previous_epoch: self.epoch,
                    new_epoch: pending,
                };
            }
            return HelloDecision::RejectEpochAndAlarm {
                current_epoch: pending,
                offered_epoch: epoch,
            };
        }
        if epoch != self.epoch {
            self.pending_epoch = Some(epoch);
            return HelloDecision::FullResync {
                previous_epoch: self.epoch,
                new_epoch: epoch,
            };
        }
        if source_head < self.applied_seq {
            return HelloDecision::RejectAndAlarm {
                applied_seq: self.applied_seq,
                offered_seq: source_head,
            };
        }
        HelloDecision::Incremental {
            after_seq: self.applied_seq,
        }
    }

    pub fn admit_batch(
        &self,
        epoch: SourceEpoch,
        after_seq: u64,
        after_chain: &ChainHash,
        through_seq: u64,
    ) -> BatchDecision {
        if epoch != self.epoch || self.pending_epoch.is_some() {
            return BatchDecision::FullResyncRequired;
        }
        if through_seq <= self.applied_seq {
            return BatchDecision::AlreadyApplied;
        }
        if after_seq > self.applied_seq {
            return BatchDecision::Gap {
                expected_at_most: self.applied_seq,
                received: after_seq,
            };
        }
        if after_seq < self.applied_seq {
            return BatchDecision::Stale {
                applied_seq: self.applied_seq,
            };
        }
        if *after_chain != self.applied_chain {
            return BatchDecision::HistoryFork {
                applied_seq: self.applied_seq,
                expected: self.applied_chain,
                offered: *after_chain,
            };
        }
        BatchDecision::Apply
    }

    /// Advance to `through_seq` with the chain hash certified for it.
    pub fn applied(&mut self, through_seq: u64, through_chain: ChainHash) {
        if through_seq > self.applied_seq {
            self.applied_seq = through_seq;
            self.applied_chain = through_chain;
        }
    }

    pub fn snapshot_applied(
        &mut self,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
    ) -> bool {
        if self.pending_epoch != Some(epoch) {
            return false;
        }
        self.retired_epochs.insert(self.epoch);
        self.epoch = epoch;
        self.applied_seq = through_seq;
        self.applied_chain = through_chain;
        self.pending_epoch = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(n: u64) -> SourceEpoch {
        SourceEpoch::random_v4(n, !n)
    }

    #[test]
    fn epoch_round_trips_as_a_uuid() {
        let epoch = epoch(7);
        assert_eq!(epoch.to_string().parse::<SourceEpoch>().unwrap(), epoch);
        assert_eq!(
            serde_json::from_str::<SourceEpoch>(&serde_json::to_string(&epoch).unwrap()).unwrap(),
            epoch
        );
        assert!("éééééééééééééééééé".parse::<SourceEpoch>().is_err());
    }

    #[test]
    fn journal_change_bumps_epoch_but_host_move_does_not_affect_binding() {
        let mut source = SourceIncarnation {
            binding: SourceBinding {
                source_id: SourceId::new(4),
                volume_serial: 99,
                root_path: "G:\\work".into(),
            },
            epoch: epoch(1),
            journal_id: Some(10),
        };
        assert!(source.binding.matches(99, "g:\\WORK"));
        assert_eq!(
            source.observe_journal(11, epoch(2)).unwrap(),
            JournalObservation::EpochBumped {
                old_epoch: epoch(1),
                new_epoch: epoch(2)
            }
        );
        assert_eq!(
            source.observe_journal(12, epoch(2)),
            Err(IdentityError::ReusedEpoch)
        );
    }

    #[test]
    fn admission_fences_rewind_gap_duplicate_and_epoch_change() {
        let mut state = AdmissionState::new(SourceId::new(3), epoch(1));
        let h8 = [8u8; 32];
        state.applied(8, h8);
        assert_eq!(
            state.admit_hello(epoch(1), 7),
            HelloDecision::RejectAndAlarm {
                applied_seq: 8,
                offered_seq: 7
            }
        );
        assert_eq!(
            state.admit_batch(epoch(1), 0, &CHAIN_GENESIS, 8),
            BatchDecision::AlreadyApplied
        );
        assert_eq!(
            state.admit_batch(epoch(1), 9, &h8, 10),
            BatchDecision::Gap {
                expected_at_most: 8,
                received: 9
            }
        );
        assert_eq!(
            state.admit_batch(epoch(1), 8, &[9u8; 32], 12),
            BatchDecision::HistoryFork {
                applied_seq: 8,
                expected: h8,
                offered: [9u8; 32]
            }
        );
        assert_eq!(
            state.admit_batch(epoch(1), 5, &CHAIN_GENESIS, 12),
            BatchDecision::Stale { applied_seq: 8 }
        );
        assert_eq!(
            state.admit_batch(epoch(1), 8, &h8, 12),
            BatchDecision::Apply
        );
        assert!(matches!(
            state.admit_hello(epoch(2), 1),
            HelloDecision::FullResync { .. }
        ));
        assert_eq!(
            state.admit_batch(epoch(2), 0, &CHAIN_GENESIS, 1),
            BatchDecision::FullResyncRequired
        );
        assert!(state.snapshot_applied(epoch(2), 1, [1u8; 32]));
        assert_eq!(state.epoch, epoch(2));
        assert_eq!(state.applied_seq, 1);
        assert_eq!(
            state.admit_hello(epoch(1), 99),
            HelloDecision::RejectEpochAndAlarm {
                current_epoch: epoch(2),
                offered_epoch: epoch(1)
            }
        );
    }

    #[test]
    fn admission_decisions_hold_across_small_sequence_space() {
        for applied in 0..32 {
            for offered in 0..48 {
                let mut state = AdmissionState::new(SourceId::new(9), epoch(1));
                let chain = [applied as u8; 32];
                state.applied(applied, chain);
                let hello = state.admit_hello(epoch(1), offered);
                if offered < applied {
                    assert!(matches!(hello, HelloDecision::RejectAndAlarm { .. }));
                } else {
                    assert_eq!(hello, HelloDecision::Incremental { after_seq: applied });
                }
                assert_eq!(
                    state.admit_batch(epoch(1), applied + 1, &chain, applied + 2),
                    BatchDecision::Gap {
                        expected_at_most: applied,
                        received: applied + 1
                    }
                );
                assert_eq!(
                    state.admit_batch(epoch(1), 0, &CHAIN_GENESIS, applied),
                    BatchDecision::AlreadyApplied
                );
                assert_eq!(
                    state.admit_batch(epoch(1), applied, &chain, applied + 1),
                    BatchDecision::Apply
                );
            }
        }
    }
}
