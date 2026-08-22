//! Incremental change application with synthetic events (platform neutral).

use eidos_catalog::changes::{ChangeEvent, Checkpoint, NativeKey, ObjectSnapshot};
use eidos_catalog::jobs::NewJob;
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{
    ContentState, FailureClass, FileAttributes, IdentityConfidence, JobStage, JobState,
    NativeIdentity, ObjectKind, Priority, SourceId, SourceKind, UnixNanos,
};
use std::sync::Arc;

struct Fx {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    catalog: Arc<Catalog>,
    source: SourceId,
    serial: u64,
    next_id: u128,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/one.txt"), vec![b'1'; 100]).unwrap();
        std::fs::write(root.join("a/b/two.txt"), vec![b'2'; 200]).unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host = catalog.ensure_host("h", "windows").unwrap();
        let source = catalog
            .add_source(&NewSource {
                host_id: host,
                name: "fx".into(),
                kind: SourceKind::WindowsLocal,
                root_path: root.display().to_string(),
                aliases: vec![],
            })
            .unwrap();
        let lister = eidos_scanner::default_lister();
        run_scan(
            &catalog,
            source,
            lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
        let root_obj = catalog
            .get_source(source)
            .unwrap()
            .unwrap()
            .root_object_id
            .unwrap();
        let serial = catalog
            .get_object(root_obj)
            .unwrap()
            .unwrap()
            .native
            .expect("root identity")
            .volume_serial;
        Fx {
            _dir: dir,
            root,
            catalog,
            source,
            serial,
            next_id: 0xFFFF_0000_0000_0000_0000_0000_0000_0000,
        }
    }

    fn key_of(&self, rel: &str) -> NativeKey {
        let id = self
            .catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .expect(rel);
        let o = self.catalog.get_object(id).unwrap().unwrap();
        NativeKey::from(o.native.expect("native identity"))
    }

    fn fresh_snapshot(&mut self, kind: ObjectKind, size: u64) -> ObjectSnapshot {
        self.next_id += 1;
        ObjectSnapshot {
            native: NativeIdentity::from_u128(
                self.serial,
                self.next_id,
                IdentityConfidence::Native,
            ),
            kind,
            attributes: FileAttributes(if kind == ObjectKind::Directory {
                0x10
            } else {
                0x20
            }),
            size,
            allocated: size.div_ceil(4096) * 4096,
            link_count: 1,
            created: Some(UnixNanos::now()),
            modified: Some(UnixNanos::now()),
            changed: None,
            accessed: None,
            reparse_tag: 0,
        }
    }

    fn snapshot_of(&self, rel: &str) -> ObjectSnapshot {
        let id = self
            .catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .expect(rel);
        let o = self.catalog.get_object(id).unwrap().unwrap();
        ObjectSnapshot {
            native: o.native.unwrap(),
            kind: o.kind,
            attributes: o.attributes,
            size: o.size,
            allocated: o.allocated,
            link_count: o.link_count,
            created: o.created,
            modified: o.modified,
            changed: o.changed,
            accessed: o.accessed,
            reparse_tag: o.reparse_tag,
        }
    }

    fn agg(&self, rel: &str) -> eidos_catalog::DirectoryAggregate {
        let id = if rel.is_empty() {
            self.catalog
                .get_source(self.source)
                .unwrap()
                .unwrap()
                .root_object_id
                .unwrap()
        } else {
            self.catalog
                .resolve_relative(self.source, rel)
                .unwrap()
                .expect(rel)
        };
        self.catalog
            .directory_aggregate(id)
            .unwrap()
            .expect("aggregate")
    }

    fn exists(&self, rel: &str) -> bool {
        self.catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .is_some()
    }
}

#[test]
fn link_new_file_updates_chain_and_outbox() {
    let mut fx = Fx::new();
    let before_root = fx.agg("");
    let parent = fx.key_of("a/b");
    let snap = fx.fresh_snapshot(ObjectKind::File, 300);
    let stats = fx
        .catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent,
                name: "three.cs".into(),
                snapshot: snap.clone(),
            }],
            Some(&Checkpoint {
                kind: "usn".into(),
                value: serde_json::json!({"next_usn": 42}),
            }),
        )
        .unwrap();
    assert_eq!(stats.objects_created, 1);
    assert_eq!(stats.entries_created, 1);
    assert_eq!(stats.outbox_rows, 1);
    assert!(fx.exists("a/b/three.cs"));
    let after_root = fx.agg("");
    assert_eq!(after_root.file_count, before_root.file_count + 1);
    assert_eq!(after_root.logical_bytes, before_root.logical_bytes + 300);
    assert_eq!(after_root.content_pending, before_root.content_pending + 1);
    let b = fx.agg("a/b");
    assert_eq!(b.file_count, 2);
    let ext = fx
        .catalog
        .extension_counts(
            fx.catalog
                .resolve_relative(fx.source, "a")
                .unwrap()
                .unwrap(),
            10,
        )
        .unwrap();
    assert!(ext.iter().any(|e| e.extension == "cs" && e.count == 1));
    let (cp, _) = fx.catalog.checkpoint(fx.source).unwrap().unwrap();
    assert_eq!(cp.kind, "usn");
    assert_eq!(cp.value["next_usn"], 42);
    let outbox = fx.catalog.outbox_poll(0, 10).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].op, "content");
    fx.catalog.outbox_consume(outbox[0].seq).unwrap();
    assert_eq!(fx.catalog.outbox_pending().unwrap(), 0);
    // Idempotent re-application.
    let again = fx
        .catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent,
                name: "three.cs".into(),
                snapshot: snap,
            }],
            None,
        )
        .unwrap();
    assert_eq!(again.objects_created, 0);
    assert_eq!(again.entries_created, 0);
    assert_eq!(fx.agg("").file_count, before_root.file_count + 1);
}

#[test]
fn update_changes_generation_and_bytes() {
    let fx = Fx::new();
    let before = fx
        .catalog
        .get_object(
            fx.catalog
                .resolve_relative(fx.source, "a/one.txt")
                .unwrap()
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    let mut snap = fx.snapshot_of("a/one.txt");
    snap.size = 150;
    snap.allocated = 4096;
    snap.modified = Some(UnixNanos(before.modified.unwrap().0 + 1_000_000_000));
    let stats = fx
        .catalog
        .apply_changes(fx.source, &[ChangeEvent::Update { snapshot: snap }], None)
        .unwrap();
    assert_eq!(stats.content_changed, 1);
    let after = fx.catalog.get_object(before.id).unwrap().unwrap();
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.size, 150);
    assert_eq!(fx.agg("a").logical_bytes, 150 + 200);
    assert_eq!(fx.agg("").logical_bytes, 350);
    // Same state again: no generation bump.
    let snap2 = fx.snapshot_of("a/one.txt");
    fx.catalog
        .apply_changes(fx.source, &[ChangeEvent::Update { snapshot: snap2 }], None)
        .unwrap();
    assert_eq!(
        fx.catalog
            .get_object(before.id)
            .unwrap()
            .unwrap()
            .generation,
        before.generation + 1
    );
}

#[test]
fn rename_is_unlink_then_link_same_object() {
    let fx = Fx::new();
    let id = fx
        .catalog
        .resolve_relative(fx.source, "a/one.txt")
        .unwrap()
        .unwrap();
    let a = fx.key_of("a");
    let snap = fx.snapshot_of("a/one.txt");
    let stats = fx
        .catalog
        .apply_changes(
            fx.source,
            &[
                ChangeEvent::Unlink {
                    parent: a,
                    name: "one.txt".into(),
                },
                ChangeEvent::Link {
                    parent: a,
                    name: "uno.txt".into(),
                    snapshot: snap,
                },
            ],
            None,
        )
        .unwrap();
    assert_eq!(stats.objects_created, 0);
    assert_eq!(stats.objects_tombstoned, 0, "object survives rename");
    assert!(!fx.exists("a/one.txt"));
    assert_eq!(
        fx.catalog.resolve_relative(fx.source, "a/uno.txt").unwrap(),
        Some(id)
    );
    assert_eq!(fx.agg("a").file_count, 2);
    assert_eq!(fx.agg("a").logical_bytes, 300);
}

#[test]
fn move_directory_moves_subtree_aggregates() {
    let mut fx = Fx::new();
    // Create /c under root, then move a/b -> c/b.
    let root_key = {
        let root = fx
            .catalog
            .get_source(fx.source)
            .unwrap()
            .unwrap()
            .root_object_id
            .unwrap();
        NativeKey::from(
            fx.catalog
                .get_object(root)
                .unwrap()
                .unwrap()
                .native
                .unwrap(),
        )
    };
    let c = fx.fresh_snapshot(ObjectKind::Directory, 0);
    fx.catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent: root_key,
                name: "c".into(),
                snapshot: c.clone(),
            }],
            None,
        )
        .unwrap();
    assert_eq!(fx.agg("").dir_count, 3, "a, a/b, c");
    let b_id = fx
        .catalog
        .resolve_relative(fx.source, "a/b")
        .unwrap()
        .unwrap();
    let b_snap = fx.snapshot_of("a/b");
    let a = fx.key_of("a");
    let stats = fx
        .catalog
        .apply_changes(
            fx.source,
            &[
                ChangeEvent::Unlink {
                    parent: a,
                    name: "b".into(),
                },
                ChangeEvent::Link {
                    parent: NativeKey::from(c.native),
                    name: "b".into(),
                    snapshot: b_snap,
                },
            ],
            None,
        )
        .unwrap();
    assert_eq!(stats.objects_tombstoned, 0);
    assert_eq!(
        fx.catalog.resolve_relative(fx.source, "c/b").unwrap(),
        Some(b_id)
    );
    assert!(fx.exists("c/b/two.txt"));
    assert!(!fx.exists("a/b"));
    let a_agg = fx.agg("a");
    assert_eq!(a_agg.file_count, 1);
    assert_eq!(a_agg.dir_count, 0);
    assert_eq!(a_agg.logical_bytes, 100);
    let c_agg = fx.agg("c");
    assert_eq!(c_agg.file_count, 1);
    assert_eq!(c_agg.dir_count, 1);
    assert_eq!(c_agg.logical_bytes, 200);
    let root = fx.agg("");
    assert_eq!(root.file_count, 2);
    assert_eq!(root.dir_count, 3);
    assert_eq!(root.logical_bytes, 300);
    let two = fx
        .catalog
        .get_object(
            fx.catalog
                .resolve_relative(fx.source, "c/b/two.txt")
                .unwrap()
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(two.generation, 1, "moved file keeps its generation");
}

#[test]
fn delete_directory_cascades() {
    let fx = Fx::new();
    let b_key = fx.key_of("a/b");
    let two_id = fx
        .catalog
        .resolve_relative(fx.source, "a/b/two.txt")
        .unwrap()
        .unwrap();
    let stats = fx
        .catalog
        .apply_changes(fx.source, &[ChangeEvent::Delete { object: b_key }], None)
        .unwrap();
    assert_eq!(stats.objects_tombstoned, 2);
    assert!(!fx.exists("a/b"));
    assert!(!fx.exists("a/b/two.txt"));
    assert!(fx
        .catalog
        .get_object(two_id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert_eq!(fx.agg("a").file_count, 1);
    assert_eq!(fx.agg("a").dir_count, 0);
    assert_eq!(fx.agg("").logical_bytes, 100);
    let outbox = fx.catalog.outbox_poll(0, 10).unwrap();
    assert!(outbox.iter().filter(|o| o.op == "delete").count() >= 2);
}

#[test]
fn unlink_without_relink_tombstones_orphan() {
    let fx = Fx::new();
    let a = fx.key_of("a");
    let id = fx
        .catalog
        .resolve_relative(fx.source, "a/one.txt")
        .unwrap()
        .unwrap();
    let stats = fx
        .catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Unlink {
                parent: a,
                name: "one.txt".into(),
            }],
            None,
        )
        .unwrap();
    assert_eq!(stats.objects_tombstoned, 1);
    assert!(fx
        .catalog
        .get_object(id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert_eq!(fx.agg("a").file_count, 1);
}

#[test]
fn hard_link_second_entry_same_object() {
    let fx = Fx::new();
    let id = fx
        .catalog
        .resolve_relative(fx.source, "a/one.txt")
        .unwrap()
        .unwrap();
    let b = fx.key_of("a/b");
    let snap = fx.snapshot_of("a/one.txt");
    fx.catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent: b,
                name: "one-link.txt".into(),
                snapshot: snap,
            }],
            None,
        )
        .unwrap();
    assert_eq!(
        fx.catalog
            .resolve_relative(fx.source, "a/b/one-link.txt")
            .unwrap(),
        Some(id)
    );
    assert_eq!(fx.catalog.get_object(id).unwrap().unwrap().link_count, 2);
    assert_eq!(fx.agg("a/b").file_count, 2);
    assert_eq!(fx.agg("a").file_count, 3, "apparent size counts per entry");
}

#[test]
fn out_of_scope_parent_is_ignored() {
    let mut fx = Fx::new();
    let stranger = NativeKey {
        volume_serial: fx.serial,
        id: 0xDEAD,
    };
    let snap = fx.fresh_snapshot(ObjectKind::File, 10);
    let stats = fx
        .catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent: stranger,
                name: "x".into(),
                snapshot: snap,
            }],
            None,
        )
        .unwrap();
    assert_eq!(stats.unmatched_parent, 1);
    assert_eq!(stats.objects_created, 0);
}

#[test]
fn checkpoint_survives_reopen_and_rescan_keeps_incremental_rows() {
    let mut fx = Fx::new();
    let parent = fx.key_of("a");
    let snap = fx.fresh_snapshot(ObjectKind::File, 7);
    fx.catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent,
                name: "ghost.txt".into(),
                snapshot: snap,
            }],
            Some(&Checkpoint {
                kind: "usn".into(),
                value: serde_json::json!({"journal_id": 1, "next_usn": 99}),
            }),
        )
        .unwrap();
    let path = fx.catalog.path().to_path_buf();
    drop(fx.catalog);
    let reopened = Catalog::open(&path).unwrap();
    let (cp, _) = reopened.checkpoint(SourceId(1)).unwrap().unwrap();
    assert_eq!(cp.value["next_usn"], 99);
    // A full rescan (the ghost file does not exist on disk) tombstones it.
    std::fs::write(fx.root.join("a/real.txt"), b"r").unwrap();
    let lister = eidos_scanner::default_lister();
    let s = run_scan(
        &reopened,
        SourceId(1),
        lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    assert_eq!(s.tombstoned_objects, 1);
    assert!(reopened
        .resolve_relative(SourceId(1), "a/ghost.txt")
        .unwrap()
        .is_none());
    assert!(reopened
        .resolve_relative(SourceId(1), "a/real.txt")
        .unwrap()
        .is_some());
}

#[test]
fn job_queue_lifecycle() {
    let fx = Fx::new();
    let obj = fx
        .catalog
        .resolve_relative(fx.source, "a/one.txt")
        .unwrap()
        .unwrap();
    let job = NewJob {
        source_id: fx.source,
        object_id: Some(obj),
        object_generation: 1,
        stage: JobStage::ContentText,
        priority: Priority::SmallText,
        idempotency_key: NewJob::object_key(JobStage::ContentText, obj, 1),
        payload: None,
        estimated_cost: 100,
    };
    let id = fx.catalog.enqueue(&job).unwrap().unwrap();
    assert!(fx.catalog.enqueue(&job).unwrap().is_none(), "idempotent");
    let counts = fx.catalog.job_counts(None).unwrap();
    assert_eq!(counts.queued, 1);

    // Higher priority job is claimed first.
    let urgent = NewJob {
        priority: Priority::CatalogCritical,
        idempotency_key: "urgent".into(),
        object_id: None,
        ..job.clone()
    };
    fx.catalog.enqueue(&urgent).unwrap();
    let claimed = fx
        .catalog
        .claim_job(&[JobStage::ContentText], "w1")
        .unwrap()
        .unwrap();
    assert_eq!(claimed.idempotency_key, "urgent");
    assert_eq!(claimed.state, JobState::Running);
    fx.catalog.complete_job(claimed.id).unwrap();

    let claimed = fx
        .catalog
        .claim_job(&[JobStage::ContentText], "w1")
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, id);
    // Transient failure re-queues with backoff (not immediately claimable).
    let st = fx
        .catalog
        .fail_job(id, FailureClass::Transient, "busy")
        .unwrap();
    assert_eq!(st, JobState::Queued);
    assert!(fx
        .catalog
        .claim_job(&[JobStage::ContentText], "w1")
        .unwrap()
        .is_none());
    // Deterministic failure is terminal.
    let st = fx
        .catalog
        .fail_job(id, FailureClass::Deterministic, "bad")
        .unwrap();
    assert_eq!(st, JobState::Failed);
    assert_eq!(fx.catalog.recent_failed_jobs(5).unwrap().len(), 1);

    // New generation supersedes the queued job of the old generation.
    let old = NewJob {
        idempotency_key: "old-gen".into(),
        object_generation: 1,
        ..job.clone()
    };
    let old_id = fx.catalog.enqueue(&old).unwrap().unwrap();
    let new = NewJob {
        idempotency_key: "new-gen".into(),
        object_generation: 2,
        ..job.clone()
    };
    fx.catalog.enqueue(&new).unwrap().unwrap();
    assert_eq!(
        fx.catalog.get_job(old_id).unwrap().unwrap().state,
        JobState::Superseded
    );

    // Crash recovery re-queues running jobs.
    let c = fx
        .catalog
        .claim_job(&[JobStage::ContentText], "w2")
        .unwrap()
        .unwrap();
    assert_eq!(c.idempotency_key, "new-gen");
    assert_eq!(fx.catalog.requeue_running_jobs().unwrap(), 1);
    assert_eq!(
        fx.catalog.get_job(c.id).unwrap().unwrap().state,
        JobState::Queued
    );
    let _ = ContentState::Pending;
}
