//! Volatile read-ahead over irrelevant USN records. Relevant changes still
//! commit with their checkpoint immediately. A crash replays only ignored
//! records; it never loses an unapplied source change.

use crate::watcher::UsnCheckpoint;
use std::time::{Duration, Instant};

const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_LAG: i64 = 16 * 1024 * 1024;
/// How long a batch may keep failing to translate before the watcher stops
/// retrying that position. Retaining the checkpoint is the safe response to a
/// failure that might clear; a failure that never clears must not retry the
/// same position forever against a volume that is already answering badly.
const RETRY_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FailedBatch {
    /// Keep the checkpoint and read the same position again.
    Retry,
    /// Stop retrying this position and reconcile instead.
    Reconcile { failing_for: Duration },
}

/// Bounded retry for a batch that will not translate.
pub(crate) struct BatchRetry {
    failing_since: Option<Instant>,
}

impl BatchRetry {
    pub fn new() -> Self {
        Self {
            failing_since: None,
        }
    }

    /// A batch translated, so any earlier failure is over and a later one
    /// starts its own window.
    pub fn succeeded(&mut self) {
        self.failing_since = None;
    }

    pub fn failed(&mut self, now: Instant) -> FailedBatch {
        let since = *self.failing_since.get_or_insert(now);
        let failing_for = now.saturating_duration_since(since);
        if failing_for < RETRY_WINDOW {
            return FailedBatch::Retry;
        }
        // Reconciliation replaces the checkpoint, so the next failure is a new
        // problem rather than a continuation of this one.
        self.failing_since = None;
        FailedBatch::Reconcile { failing_for }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CheckpointPlan {
    Unchanged,
    Deferred,
    Persist,
    InvalidPosition,
}

pub(crate) struct ReadAhead {
    durable: Option<UsnCheckpoint>,
    next: i64,
    saved_at: Instant,
}

impl ReadAhead {
    pub fn new(now: Instant) -> Self {
        Self {
            durable: None,
            next: 0,
            saved_at: now,
        }
    }

    /// A scan/recovery/checkpoint replacement invalidates all read-ahead.
    /// Return true so the caller also reopens the matching journal handle.
    pub fn synchronize(&mut self, durable: &UsnCheckpoint, now: Instant) -> bool {
        if self.durable.as_ref() == Some(durable) {
            return false;
        }
        self.committed(durable, now);
        true
    }

    pub fn next_usn(&self) -> i64 {
        self.next
    }

    /// Called only after translation succeeded. Deferral changes no catalog
    /// state and emits no per-batch log (which could itself feed the journal).
    pub fn plan(&mut self, next: i64, must_persist: bool, now: Instant) -> CheckpointPlan {
        let Some(durable) = &self.durable else {
            return CheckpointPlan::InvalidPosition;
        };
        if next < self.next {
            return CheckpointPlan::InvalidPosition;
        }
        if must_persist {
            return CheckpointPlan::Persist;
        }
        if next == durable.next_usn {
            return CheckpointPlan::Unchanged;
        }
        if next.saturating_sub(durable.next_usn) >= MAX_LAG
            || now.saturating_duration_since(self.saved_at) >= FLUSH_INTERVAL
        {
            // Do not consume this batch before its compare-and-swap succeeds.
            return CheckpointPlan::Persist;
        }
        self.next = next;
        CheckpointPlan::Deferred
    }

    /// Only a successful durable compare-and-swap may acknowledge this position.
    pub fn committed(&mut self, checkpoint: &UsnCheckpoint, now: Instant) {
        self.durable = Some(checkpoint.clone());
        self.next = checkpoint.next_usn;
        self.saved_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checkpoint(next_usn: i64) -> UsnCheckpoint {
        UsnCheckpoint {
            journal_id: 1,
            next_usn,
            volume_root: "fixture-volume".into(),
        }
    }

    #[test]
    fn thousands_of_irrelevant_batches_create_no_checkpoint_writes() {
        let now = Instant::now();
        let cp = checkpoint(100);
        let mut pending = ReadAhead::new(now);
        pending.synchronize(&cp, now);
        for next in 101..10_101 {
            assert_eq!(pending.plan(next, false, now), CheckpointPlan::Deferred);
            assert!(!pending.synchronize(&cp, now));
            assert_eq!(pending.next_usn(), next);
        }
    }

    #[test]
    fn real_changes_are_immediate_and_failed_writes_do_not_skip_them() {
        let now = Instant::now();
        let mut pending = ReadAhead::new(now);
        pending.synchronize(&checkpoint(100), now);
        assert_eq!(pending.plan(200, false, now), CheckpointPlan::Deferred);
        assert_eq!(pending.plan(300, true, now), CheckpointPlan::Persist);
        assert_eq!(
            pending.next_usn(),
            200,
            "failed write must retry the relevant batch"
        );
        pending.committed(&checkpoint(300), now);
        assert_eq!(pending.next_usn(), 300);
    }

    #[test]
    fn age_and_byte_bounds_flush_without_a_quiet_idle_timer() {
        let now = Instant::now();
        let mut pending = ReadAhead::new(now);
        pending.synchronize(&checkpoint(100), now);
        assert_eq!(
            pending.plan(200, false, now + FLUSH_INTERVAL),
            CheckpointPlan::Persist
        );
        assert_eq!(pending.next_usn(), 100);
        assert_eq!(
            pending.plan(100 + MAX_LAG, false, now),
            CheckpointPlan::Persist
        );
        pending.committed(&checkpoint(100 + MAX_LAG), now);
        assert_eq!(
            pending.plan(101 + MAX_LAG, false, now),
            CheckpointPlan::Deferred
        );
    }

    #[test]
    fn restart_replays_ignored_records_and_replacement_discards_old_read_ahead() {
        let now = Instant::now();
        let mut pending = ReadAhead::new(now);
        pending.synchronize(&checkpoint(100), now);
        pending.plan(200, false, now);
        let mut restarted = ReadAhead::new(now);
        restarted.synchronize(&checkpoint(100), now);
        assert_eq!(restarted.next_usn(), 100);
        let mut replacement = checkpoint(50);
        replacement.journal_id = 2;
        assert!(pending.synchronize(&replacement, now));
        assert_eq!(pending.next_usn(), 50);
        replacement.volume_root = "replacement-volume".into();
        assert!(pending.synchronize(&replacement, now));
    }

    #[test]
    fn unchanged_and_regressing_positions_never_advance() {
        let now = Instant::now();
        let mut pending = ReadAhead::new(now);
        assert_eq!(
            pending.plan(100, false, now),
            CheckpointPlan::InvalidPosition
        );
        pending.synchronize(&checkpoint(100), now);
        assert_eq!(pending.plan(100, false, now), CheckpointPlan::Unchanged);
        pending.plan(200, false, now);
        assert_eq!(
            pending.plan(199, false, now),
            CheckpointPlan::InvalidPosition
        );
        assert_eq!(pending.next_usn(), 200);
    }

    #[test]
    fn a_batch_that_never_translates_reconciles_instead_of_retrying_forever() {
        let now = Instant::now();
        let mut retry = BatchRetry::new();
        assert_eq!(retry.failed(now), FailedBatch::Retry);
        assert_eq!(
            retry.failed(now + Duration::from_secs(2)),
            FailedBatch::Retry
        );
        assert_eq!(
            retry.failed(now + RETRY_WINDOW - Duration::from_millis(1)),
            FailedBatch::Retry
        );
        assert_eq!(
            retry.failed(now + RETRY_WINDOW),
            FailedBatch::Reconcile {
                failing_for: RETRY_WINDOW
            },
            "a position that has not become readable must not be retried forever"
        );
        // Reconciliation replaces the checkpoint, so the window starts over.
        assert_eq!(retry.failed(now + RETRY_WINDOW), FailedBatch::Retry);
    }

    #[test]
    fn a_translated_batch_clears_an_earlier_failure() {
        let now = Instant::now();
        let mut retry = BatchRetry::new();
        assert_eq!(retry.failed(now), FailedBatch::Retry);
        retry.succeeded();
        // A failure two hours later is its own problem, not a continuation.
        assert_eq!(
            retry.failed(now + Duration::from_secs(7200)),
            FailedBatch::Retry,
            "an intervening success must not leave a stale failure window"
        );
    }

    #[test]
    fn catalog_fencing_and_failed_persistence_preserve_the_retry_position() {
        use eidos_catalog::{Catalog, NewSource};
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host_id = catalog.ensure_host("fixture", "test").unwrap();
        let source = catalog
            .add_source(&NewSource {
                host_id,
                name: "fixture".into(),
                kind: eidos_domain::SourceKind::WindowsGeneric,
                root_path: "synthetic-root".into(),
                aliases: vec![],
            })
            .unwrap();
        let cp = checkpoint(100);
        catalog.set_checkpoint(source, &cp.to_checkpoint()).unwrap();
        let now = Instant::now();
        let mut pending = ReadAhead::new(now);
        pending.synchronize(&cp, now);
        let before = catalog.writer_stats().acquisitions;
        for next in 101..201 {
            assert_eq!(pending.plan(next, false, now), CheckpointPlan::Deferred);
            let (stored, _) = catalog.checkpoint(source).unwrap().unwrap();
            assert_eq!(stored, cp.to_checkpoint());
        }
        assert_eq!(catalog.writer_stats().acquisitions, before);
        assert_eq!(pending.plan(300, true, now), CheckpointPlan::Persist);
        catalog.with_writer(|conn| { conn.execute_batch("CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON sources BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")?; Ok(()) }).unwrap();
        assert!(catalog
            .advance_feed_checkpoint(
                source,
                &cp.to_checkpoint(),
                &checkpoint(300).to_checkpoint()
            )
            .is_err());
        assert_eq!(pending.next_usn(), 200);
        assert_eq!(
            catalog.checkpoint(source).unwrap().unwrap().0,
            cp.to_checkpoint()
        );
        catalog
            .with_writer(|conn| {
                conn.execute_batch("DROP TRIGGER reject_checkpoint")?;
                Ok(())
            })
            .unwrap();
        // A rescan won the checkpoint while the old batch was in flight.
        let replacement = checkpoint(500);
        catalog
            .set_checkpoint(source, &replacement.to_checkpoint())
            .unwrap();
        assert!(!catalog
            .advance_feed_checkpoint(
                source,
                &cp.to_checkpoint(),
                &checkpoint(300).to_checkpoint()
            )
            .unwrap());
        assert!(pending.synchronize(&replacement, now));
        assert_eq!(pending.next_usn(), 500);
        assert_eq!(
            catalog.checkpoint(source).unwrap().unwrap().0,
            replacement.to_checkpoint()
        );
    }
}
