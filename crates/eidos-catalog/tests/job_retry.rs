//! Operator retry controls for failed jobs (`eidos_catalog::retry`).

use eidos_catalog::jobs::{NewJob, MAX_TRANSIENT_ATTEMPTS};
use eidos_catalog::retry::RetrySelector;
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{
    FailureClass, JobId, JobStage, JobState, ObjectId, Priority, SourceId, SourceKind, SourceState,
};
use std::path::PathBuf;
use std::sync::Arc;

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<Catalog>,
    source: SourceId,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        for (name, size) in [("one.txt", 100), ("two.txt", 200), ("three.txt", 300)] {
            std::fs::write(root.join(name), vec![b'x'; size]).unwrap();
        }
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host = catalog.ensure_host("h", "windows").unwrap();
        let source = catalog
            .add_source(&NewSource {
                host_id: host,
                name: "fx".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: root.display().to_string(),
                aliases: vec![],
            })
            .unwrap();
        let fx = Fx {
            _dir: dir,
            root,
            catalog,
            source,
        };
        fx.rescan();
        fx
    }

    fn rescan(&self) {
        let lister = eidos_scanner::default_lister();
        run_scan(
            &self.catalog,
            self.source,
            lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
    }

    fn obj(&self, rel: &str) -> ObjectId {
        self.catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .expect(rel)
    }

    fn generation(&self, obj: ObjectId) -> u32 {
        self.catalog.get_object(obj).unwrap().unwrap().generation
    }

    /// Queue a content job for the object's current generation.
    fn queue(&self, rel: &str) -> JobId {
        let obj = self.obj(rel);
        let gen = self.generation(obj);
        let size = self.catalog.get_object(obj).unwrap().unwrap().size;
        self.catalog
            .enqueue(&NewJob {
                source_id: self.source,
                object_id: Some(obj),
                object_generation: gen,
                stage: JobStage::ContentText,
                priority: Priority::SmallText,
                idempotency_key: NewJob::object_key(JobStage::ContentText, obj, gen),
                payload: None,
                estimated_cost: size,
            })
            .unwrap()
            .expect("queued")
    }

    /// Queue a job, mark it claimed the way a worker would (so `attempts`
    /// is realistic regardless of what else is queued), then fail it.
    fn fail(&self, rel: &str, class: FailureClass, error: &str) -> JobId {
        let id = self.queue(rel);
        self.catalog
            .with_writer(|c| {
                c.execute(
                    "UPDATE jobs SET state = 'running', attempts = attempts + 1, worker = 'w' WHERE job_id = ?1",
                    rusqlite::params![id.0],
                )?;
                Ok(())
            })
            .unwrap();
        self.catalog.fail_job(id, class, error).unwrap();
        id
    }

    fn state(&self, id: JobId) -> JobState {
        self.catalog.get_job(id).unwrap().unwrap().state
    }

    fn active_jobs(&self, obj: ObjectId) -> u64 {
        self.catalog
            .with_reader(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM jobs WHERE object_id = ?1 AND state IN ('queued','running')",
                    rusqlite::params![obj.0],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap() as u64
    }
}

#[test]
fn single_retry_previews_preserves_history_and_stays_idempotent() {
    let fx = Fx::new();
    let obj = fx.obj("one.txt");
    let id = fx.fail("one.txt", FailureClass::Deterministic, "parse: bad utf-16");
    assert_eq!(fx.state(id), JobState::Failed);

    // Preview changes nothing but reports what would happen.
    let p = fx
        .catalog
        .retry_failed_jobs(&RetrySelector {
            preview: true,
            ..RetrySelector::job(id)
        })
        .unwrap();
    assert!(p.preview);
    assert_eq!((p.accepted, p.skipped, p.rejected), (1, 0, 0));
    assert_eq!(p.bytes, 100, "object size travels as estimated_cost");
    assert_eq!(fx.state(id), JobState::Failed, "preview must not act");
    assert_eq!(fx.catalog.get_job(id).unwrap().unwrap().requeue_count, 0);

    let r = fx
        .catalog
        .retry_failed_jobs(&RetrySelector::job(id))
        .unwrap();
    assert!(!r.preview);
    assert_eq!((r.accepted, r.skipped, r.rejected, r.bytes), (1, 0, 0, 100));
    assert_eq!(r.job_ids, vec![id]);
    let job = fx.catalog.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.attempts, 1, "attempt history is preserved");
    assert_eq!(
        job.last_error.as_deref(),
        Some("parse: bad utf-16"),
        "error history is preserved"
    );
    assert_eq!(job.failure_class, Some(FailureClass::Deterministic));
    assert_eq!(job.requeue_count, 1);
    assert!(job.requeued_at.is_some());
    assert!(job.worker.is_none() && job.finished_at.is_none());

    // Requeueing again (operator double-click, restart, retry racing the
    // queue) never produces a second active job.
    let again = fx
        .catalog
        .retry_failed_jobs(&RetrySelector::job(id))
        .unwrap();
    assert_eq!((again.accepted, again.rejected), (0, 1));
    assert_eq!(again.rejected_reasons.get("queued"), Some(&1));
    assert_eq!(fx.active_jobs(obj), 1);

    // A job of another object that is already active blocks the retry.
    let other = fx.fail("two.txt", FailureClass::Corrupt, "corrupt zip");
    let obj2 = fx.obj("two.txt");
    fx.catalog
        .enqueue(&NewJob {
            source_id: fx.source,
            object_id: Some(obj2),
            object_generation: fx.generation(obj2),
            stage: JobStage::ContentText,
            priority: Priority::SmallText,
            idempotency_key: "manual-duplicate".into(),
            payload: None,
            estimated_cost: 200,
        })
        .unwrap()
        .expect("queued");
    let dup = fx
        .catalog
        .retry_failed_jobs(&RetrySelector::job(other))
        .unwrap();
    assert_eq!((dup.accepted, dup.rejected), (0, 1));
    assert_eq!(dup.rejected_reasons.get("already_active"), Some(&1));
    assert_eq!(fx.state(other), JobState::Failed);
    assert_eq!(fx.active_jobs(obj2), 1);
}

#[test]
fn running_jobs_are_rejected_not_interrupted() {
    let fx = Fx::new();
    let id = fx.queue("one.txt");
    let claimed = fx
        .catalog
        .claim_job(&[JobStage::ContentText], "w")
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, id);
    let r = fx
        .catalog
        .retry_failed_jobs(&RetrySelector::job(id))
        .unwrap();
    assert_eq!((r.accepted, r.skipped, r.rejected), (0, 0, 1));
    assert_eq!(r.rejected_reasons.get("running"), Some(&1));
    assert_eq!(fx.state(id), JobState::Running, "worker keeps the job");
}

#[test]
fn bulk_retry_filters_by_class_and_reason_prefix() {
    let fx = Fx::new();
    let det = fx.fail(
        "one.txt",
        FailureClass::Deterministic,
        "extract: no decoder",
    );
    let corrupt = fx.fail("two.txt", FailureClass::Corrupt, "sink: index write failed");
    let limit = fx.fail(
        "three.txt",
        FailureClass::ResourceLimit,
        "extract: over 64 MiB",
    );

    let all = RetrySelector::source(fx.source, JobStage::ContentText);
    let preview = fx
        .catalog
        .retry_failed_jobs(&RetrySelector {
            preview: true,
            ..all.clone()
        })
        .unwrap();
    assert_eq!(preview.accepted, 3);
    assert_eq!(preview.bytes, 600, "100 + 200 + 300");
    assert_eq!(
        fx.catalog
            .failed_jobs_by_source(JobStage::ContentText)
            .unwrap()[&fx.source],
        (3, 600)
    );

    // One class only.
    let r = fx
        .catalog
        .retry_failed_jobs(&RetrySelector {
            class: Some(FailureClass::Corrupt),
            ..all.clone()
        })
        .unwrap();
    assert_eq!((r.accepted, r.bytes), (1, 200));
    assert_eq!(fx.state(corrupt), JobState::Queued);
    assert_eq!(fx.state(det), JobState::Failed);

    // One reason prefix (matched literally: `_` is not a wildcard).
    let r = fx
        .catalog
        .retry_failed_jobs(&RetrySelector {
            reason_prefix: Some("extract: over".into()),
            ..all.clone()
        })
        .unwrap();
    assert_eq!((r.accepted, r.bytes), (1, 300));
    assert_eq!(fx.state(limit), JobState::Queued);
    assert_eq!(fx.state(det), JobState::Failed);
    let none = fx
        .catalog
        .retry_failed_jobs(&RetrySelector {
            reason_prefix: Some("extr_ct:".into()),
            ..all.clone()
        })
        .unwrap();
    assert_eq!(none.total(), 0);

    // The rest, and the two already-queued jobs are not candidates again.
    let r = fx.catalog.retry_failed_jobs(&all).unwrap();
    assert_eq!((r.accepted, r.skipped, r.rejected), (1, 0, 0));
    assert_eq!(fx.state(det), JobState::Queued);
    assert_eq!(
        fx.catalog
            .failed_jobs_by_source(JobStage::ContentText)
            .unwrap()
            .get(&fx.source),
        None
    );
}

#[test]
fn deleted_stale_retired_and_disabled_objects_are_skipped() {
    let fx = Fx::new();
    let gone = fx.fail("one.txt", FailureClass::Deterministic, "boom");
    let stale = fx.fail("two.txt", FailureClass::Deterministic, "boom");
    let ok = fx.fail("three.txt", FailureClass::Deterministic, "boom");

    std::fs::remove_file(fx.root.join("one.txt")).unwrap();
    std::fs::write(fx.root.join("two.txt"), vec![b'y'; 250]).unwrap();
    fx.rescan();
    let stale_job = fx.catalog.get_job(stale).unwrap().unwrap();
    assert!(
        fx.generation(fx.obj("two.txt")) > stale_job.object_generation,
        "rescan bumped the object generation"
    );

    let all = RetrySelector::source(fx.source, JobStage::ContentText);
    let r = fx.catalog.retry_failed_jobs(&all).unwrap();
    assert_eq!((r.accepted, r.skipped, r.rejected), (1, 2, 0));
    assert_eq!(r.skipped_reasons.get("deleted"), Some(&1));
    assert_eq!(r.skipped_reasons.get("stale_generation"), Some(&1));
    assert_eq!(fx.state(ok), JobState::Queued);
    assert_eq!(fx.state(gone), JobState::Failed);
    assert_eq!(fx.state(stale), JobState::Failed);

    // Content extraction turned off for the source: nothing to requeue into.
    fx.catalog
        .fail_job(ok, FailureClass::Deterministic, "boom")
        .unwrap();
    fx.catalog.set_content_policy(fx.source, false, 1).unwrap();
    let r = fx.catalog.retry_failed_jobs(&all).unwrap();
    assert_eq!(r.skipped_reasons.get("content_disabled"), Some(&1));
    fx.catalog.set_content_policy(fx.source, true, 1).unwrap();

    // A retired source is never requeued.
    fx.catalog
        .set_source_state(fx.source, SourceState::Retired, None)
        .unwrap();
    let r = fx.catalog.retry_failed_jobs(&all).unwrap();
    assert_eq!((r.accepted, r.skipped), (0, 3));
    assert_eq!(r.skipped_reasons.get("retired"), Some(&3));
}

#[test]
fn operator_retry_restores_the_transient_backoff_budget() {
    let fx = Fx::new();
    let id = fx.queue("one.txt");
    // Automatic backoff burns the transient budget without operator help.
    for i in 0..MAX_TRANSIENT_ATTEMPTS {
        fx.catalog
            .with_writer(|c| {
                c.execute(
                    "UPDATE jobs SET attempts = ?2, state = 'running' WHERE job_id = ?1",
                    rusqlite::params![id.0, (i + 1) as i64],
                )?;
                Ok(())
            })
            .unwrap();
        let st = fx
            .catalog
            .fail_job(id, FailureClass::Transient, "share offline")
            .unwrap();
        assert_eq!(
            st,
            if i + 1 < MAX_TRANSIENT_ATTEMPTS {
                JobState::Queued
            } else {
                JobState::Failed
            }
        );
    }
    let job = fx.catalog.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.attempts, MAX_TRANSIENT_ATTEMPTS);

    // After the share is back the operator retries: history is kept and the
    // automatic schedule starts over from that baseline.
    let r = fx
        .catalog
        .retry_failed_jobs(&RetrySelector::job(id))
        .unwrap();
    assert_eq!(r.accepted, 1);
    let job = fx.catalog.get_job(id).unwrap().unwrap();
    assert_eq!(job.attempts, MAX_TRANSIENT_ATTEMPTS, "history kept");
    fx.catalog
        .with_writer(|c| {
            c.execute(
                "UPDATE jobs SET attempts = attempts + 1, state = 'running' WHERE job_id = ?1",
                rusqlite::params![id.0],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        fx.catalog
            .fail_job(id, FailureClass::Transient, "share offline")
            .unwrap(),
        JobState::Queued,
        "the retried job retries automatically again"
    );
}
