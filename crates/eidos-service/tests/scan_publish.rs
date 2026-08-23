//! Native scan publication: the changes that overlapped enumeration are
//! replayed into the still-open generation, and the checkpoint commits in
//! the same transaction as the publication. The replay feed is scripted, so
//! these tests need neither a USN journal nor elevation.

use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
use eidos_catalog::NewSource;
use eidos_domain::{
    FileAttributes, IdentityConfidence, NativeIdentity, ObjectKind, SourceId, SourceKind,
    SourceState, UnixNanos,
};
use eidos_service::scanner::{enumerate, run_full_scan, ScanProgress};
use eidos_service::state::AppState;
use eidos_service::watcher::{replay_and_publish, OverlapFeed, ReplayStep, UsnCheckpoint};
use eidos_service::ServiceConfig;
use std::collections::VecDeque;
use std::sync::Arc;

struct Fx {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    sid: SourceId,
    serial: u64,
    next_id: u128,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/readme.md"), b"hello").unwrap();
        std::fs::write(root.join("a.txt"), b"aaaa").unwrap();
        let cfg = ServiceConfig {
            data_dir: dir.path().join("data"),
            scan_threads: 2,
            auto_reconcile: false,
            content: false,
            ..Default::default()
        };
        let state = Arc::new(AppState::open(&cfg).unwrap());
        let sid = state
            .catalog
            .add_source(&NewSource {
                host_id: state.host_id,
                name: "publish-fixture".into(),
                kind: SourceKind::WindowsLocal,
                root_path: root.display().to_string(),
                aliases: vec![],
            })
            .unwrap();
        // Generation 1: a plain published scan.
        let progress = ScanProgress::new(sid);
        let s = run_full_scan(&state, sid, &progress).unwrap();
        assert!(s.published);
        assert_eq!(s.generation, 1);
        let fx = Fx {
            _dir: dir,
            state,
            sid,
            serial: 0,
            next_id: 0xFFFF_0000_0000_0000_0000_0000_0000_0000,
        };
        let serial = fx.key_of("").volume_serial;
        Fx { serial, ..fx }
    }

    fn source(&self) -> eidos_catalog::model::SourceRecord {
        self.state.catalog.get_source(self.sid).unwrap().unwrap()
    }

    fn key_of(&self, rel: &str) -> NativeKey {
        let id = if rel.is_empty() {
            self.source().root_object_id.unwrap()
        } else {
            self.state
                .catalog
                .resolve_relative(self.sid, rel)
                .unwrap()
                .expect(rel)
        };
        let o = self.state.catalog.get_object(id).unwrap().unwrap();
        NativeKey::from(o.native.expect("native identity"))
    }

    fn exists(&self, rel: &str) -> bool {
        self.state
            .catalog
            .resolve_relative(self.sid, rel)
            .unwrap()
            .is_some()
    }

    fn checkpoint_usn(&self) -> Option<i64> {
        self.state
            .catalog
            .checkpoint(self.sid)
            .unwrap()
            .map(|(cp, _)| UsnCheckpoint::from_checkpoint(&cp).unwrap().next_usn)
    }

    fn fresh_file(&mut self, size: u64) -> ObjectSnapshot {
        self.next_id += 1;
        ObjectSnapshot {
            native: NativeIdentity::from_u128(
                self.serial,
                self.next_id,
                IdentityConfidence::Native,
            ),
            kind: ObjectKind::File,
            attributes: FileAttributes(0x20),
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

    fn pending_checkpoint(&self) -> UsnCheckpoint {
        UsnCheckpoint {
            journal_id: 7,
            next_usn: 10,
            volume_root: "C:\\".into(),
        }
    }
}

/// What the service advertised at the moment the feed was read.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
    state: SourceState,
    published: Option<i64>,
    checkpoint_usn: Option<i64>,
}

struct Scripted {
    steps: VecDeque<ReplayStep>,
    state: Arc<AppState>,
    sid: SourceId,
    observed: Vec<Observation>,
    positions: Vec<i64>,
}

impl Scripted {
    fn new(fx: &Fx, steps: Vec<ReplayStep>) -> Self {
        Self {
            steps: steps.into(),
            state: fx.state.clone(),
            sid: fx.sid,
            observed: Vec::new(),
            positions: Vec::new(),
        }
    }
}

impl OverlapFeed for Scripted {
    fn next(&mut self, next_usn: i64) -> ReplayStep {
        let s = self.state.catalog.get_source(self.sid).unwrap().unwrap();
        self.observed.push(Observation {
            state: s.state,
            published: s.published_generation,
            checkpoint_usn: self
                .state
                .catalog
                .checkpoint(self.sid)
                .unwrap()
                .map(|(cp, _)| UsnCheckpoint::from_checkpoint(&cp).unwrap().next_usn),
        });
        self.positions.push(next_usn);
        self.steps.pop_front().expect("feed read past its script")
    }
}

#[test]
fn overlapping_changes_publish_with_checkpoint_and_never_advertise_early() {
    let mut fx = Fx::new();
    let root = fx.key_of("");
    let a_txt = fx.key_of("a.txt");
    let late = fx.fresh_file(7);
    let mut feed = Scripted::new(
        &fx,
        vec![
            ReplayStep::Batch {
                events: vec![
                    ChangeEvent::Link {
                        parent: root,
                        name: "late.txt".into(),
                        snapshot: late,
                    },
                    ChangeEvent::Delete { object: a_txt },
                ],
                next_usn: 20,
            },
            // Records that were all out of scope still advance the position.
            ReplayStep::Batch {
                events: vec![],
                next_usn: 25,
            },
            ReplayStep::CaughtUp { next_usn: 30 },
        ],
    );
    let progress = ScanProgress::new(fx.sid);
    let session = enumerate(&fx.state, fx.sid, &progress).unwrap();
    let summary = replay_and_publish(
        &fx.state,
        fx.sid,
        session,
        fx.pending_checkpoint(),
        &mut feed,
        &progress,
    )
    .unwrap();
    assert!(summary.published);
    assert_eq!(summary.generation, 2);
    assert_eq!(progress.view().phase, "done");

    // Every feed read happened while the generation was still open.
    assert_eq!(feed.positions, vec![10, 20, 25]);
    for o in &feed.observed {
        assert_eq!(
            *o,
            Observation {
                state: SourceState::Reconciling,
                published: Some(1),
                checkpoint_usn: None,
            },
            "replay window must not advertise the new generation"
        );
    }

    let s = fx.source();
    assert_eq!(s.published_generation, Some(2));
    assert!(matches!(
        s.state,
        SourceState::MetadataComplete | SourceState::ContentPending
    ));
    assert_eq!(fx.checkpoint_usn(), Some(30));
    let (cp, _) = fx.state.catalog.checkpoint(fx.sid).unwrap().unwrap();
    assert_eq!(UsnCheckpoint::from_checkpoint(&cp).unwrap().journal_id, 7);
    assert!(fx.exists("late.txt"), "file created during enumeration");
    assert!(!fx.exists("a.txt"), "file deleted during enumeration");
    assert!(fx.exists("docs/readme.md"));
    let root_agg = fx
        .state
        .catalog
        .directory_aggregate(s.root_object_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(root_agg.file_count, 2, "readme.md + late.txt");
    assert_eq!(root_agg.logical_bytes, 12);
}

#[test]
fn replay_read_failure_aborts_and_preserves_previous_truth() {
    let mut fx = Fx::new();
    fx.state
        .catalog
        .set_checkpoint(
            fx.sid,
            &UsnCheckpoint {
                journal_id: 7,
                next_usn: 3,
                volume_root: "C:\\".into(),
            }
            .to_checkpoint(),
        )
        .unwrap();
    let root = fx.key_of("");
    let late = fx.fresh_file(7);
    let mut feed = Scripted::new(
        &fx,
        vec![
            ReplayStep::Batch {
                events: vec![ChangeEvent::Link {
                    parent: root,
                    name: "late.txt".into(),
                    snapshot: late,
                }],
                next_usn: 20,
            },
            ReplayStep::Failed("journal read failed".into()),
        ],
    );
    let progress = ScanProgress::new(fx.sid);
    let session = enumerate(&fx.state, fx.sid, &progress).unwrap();
    let err = replay_and_publish(
        &fx.state,
        fx.sid,
        session,
        fx.pending_checkpoint(),
        &mut feed,
        &progress,
    )
    .unwrap_err();
    assert!(err.to_string().contains("journal read failed"), "{err}");

    let s = fx.source();
    assert_eq!(s.published_generation, Some(1));
    assert_eq!(s.state, SourceState::Degraded);
    assert!(s
        .state_reason
        .as_deref()
        .unwrap()
        .contains("overlapped enumeration failed"));
    assert_eq!(fx.checkpoint_usn(), Some(3), "previous checkpoint kept");
    assert!(fx.exists("a.txt"));
    // Rows from replay batches that succeeded stay visible, like rows
    // ingested by the enumeration itself: they are observed truth, and the
    // `degraded` state (not row visibility) carries the incompleteness.
    assert!(fx.exists("late.txt"));
    let gens = fx.state.catalog.list_generations(fx.sid, 10).unwrap();
    assert_eq!(
        gens.iter().find(|g| g.generation == 2).unwrap().state,
        "aborted"
    );
}

#[test]
fn catalog_failure_during_replay_aborts() {
    let fx = Fx::new();
    let root = fx.key_of("");
    // Deleting the source root is refused by the catalog.
    let mut feed = Scripted::new(
        &fx,
        vec![ReplayStep::Batch {
            events: vec![ChangeEvent::Delete { object: root }],
            next_usn: 20,
        }],
    );
    let progress = ScanProgress::new(fx.sid);
    let session = enumerate(&fx.state, fx.sid, &progress).unwrap();
    let err = replay_and_publish(
        &fx.state,
        fx.sid,
        session,
        fx.pending_checkpoint(),
        &mut feed,
        &progress,
    )
    .unwrap_err();
    assert!(err.to_string().contains("apply failed"), "{err}");
    let s = fx.source();
    assert_eq!(s.published_generation, Some(1));
    assert_eq!(s.state, SourceState::Degraded);
    assert_eq!(fx.checkpoint_usn(), None);
}

#[test]
fn journal_wrap_during_enumeration_publishes_degraded_without_checkpoint() {
    let fx = Fx::new();
    fx.state
        .catalog
        .set_checkpoint(fx.sid, &fx.pending_checkpoint().to_checkpoint())
        .unwrap();
    let mut feed = Scripted::new(&fx, vec![ReplayStep::JournalInvalid]);
    let progress = ScanProgress::new(fx.sid);
    let session = enumerate(&fx.state, fx.sid, &progress).unwrap();
    let summary = replay_and_publish(
        &fx.state,
        fx.sid,
        session,
        fx.pending_checkpoint(),
        &mut feed,
        &progress,
    )
    .unwrap();
    assert!(summary.published);
    assert_eq!(summary.final_state, SourceState::Degraded);
    let s = fx.source();
    assert_eq!(s.published_generation, Some(2));
    assert_eq!(s.state, SourceState::Degraded);
    assert!(s.state_reason.as_deref().unwrap().contains("wrapped"));
    assert_eq!(
        fx.checkpoint_usn(),
        None,
        "an invalid feed position is never stored"
    );
    assert!(fx.exists("a.txt"));
}
