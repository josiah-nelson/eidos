//! Milestone 1 integration tests on project-owned temporary fixtures.
//!
//! Covers: full scan counts and aggregates, rename, hard links, deletion,
//! subtree move, unlisted directories preserving children, interrupted
//! enumeration never publishing, crash recovery, and path rendering.

use eidos_catalog::scan::{run_scan, RunScanOptions, ScanKind};
use eidos_catalog::{Catalog, ChildSort, ChildrenPage, NewSource};
use eidos_domain::{ContentState, FileAttributes, ObjectKind, SourceId, SourceKind, SourceState};
use eidos_scanner::{
    default_lister, DirEvent, DirToken, DirectoryLister, RawEntry, ScanError, ScanErrorKind,
    VolumeInfo,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<Catalog>,
    source: SourceId,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::create_dir_all(root.join("a/d")).unwrap();
    std::fs::create_dir_all(root.join("e")).unwrap();
    std::fs::write(root.join("a/one.txt"), vec![b'1'; 100]).unwrap();
    std::fs::write(root.join("a/b/two.txt"), vec![b'2'; 200]).unwrap();
    std::fs::write(root.join("a/b/c/three.cs"), vec![b'3'; 300]).unwrap();
    std::fs::write(root.join("a/b/c/three.idb"), vec![b'3'; 50]).unwrap();
    std::fs::write(root.join("e/four.md"), vec![b'4'; 400]).unwrap();
    std::fs::write(root.join("five.vhdx"), vec![b'5'; 5000]).unwrap();
    std::fs::write(root.join("empty.txt"), b"").unwrap();
    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("test-host", "windows").unwrap();
    let source = catalog
        .add_source(&NewSource {
            host_id: host,
            name: "fixture".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    Fixture {
        _dir: dir,
        root,
        catalog,
        source,
    }
}

fn scan(f: &Fixture) -> eidos_catalog::scan::ScanSummary {
    let lister = default_lister();
    run_scan(
        &f.catalog,
        f.source,
        lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap()
}

fn object_at(f: &Fixture, rel: &str) -> Option<eidos_catalog::ObjectRecord> {
    let id = f.catalog.resolve_relative(f.source, rel).unwrap()?;
    f.catalog.get_object(id).unwrap()
}

#[cfg(unix)]
#[test]
fn a_literal_backslash_in_a_posix_name_is_not_a_path_separator() {
    let f = fixture();
    let name = r"odd\name.txt";
    std::fs::write(f.root.join(name), b"odd").unwrap();
    scan(&f);

    let object = object_at(&f, name).expect("the literal-backslash name should resolve");
    let expected = f.root.join(name).display().to_string();
    assert_eq!(
        f.catalog.render_path(object.id).unwrap().as_deref(),
        Some(expected.as_str())
    );
    assert!(object_at(&f, "odd/name.txt").is_none());
}

#[test]
fn full_scan_counts_sizes_and_aggregates() {
    let f = fixture();
    let summary = scan(&f);
    assert!(summary.published);
    assert_eq!(summary.stats.dirs_listed, 6, "root, a, a/b, a/b/c, a/d, e");
    assert_eq!(summary.stats.entries_seen, 12);
    assert_eq!(summary.stats.errors, 0);

    let src = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(src.published_generation, Some(1));
    assert_eq!(src.state, SourceState::ContentPending);

    let counts = f.catalog.source_counts(f.source).unwrap();
    assert_eq!(counts.files, 7);
    assert_eq!(counts.directories, 5);
    assert_eq!(counts.logical_bytes, 100 + 200 + 300 + 50 + 400 + 5000);
    // Allocated is what the filesystem reports: cluster-rounded for
    // non-resident files, 8-byte-rounded for NTFS resident (MFT) data. It is
    // never below logical size.
    assert!(counts.allocated_bytes >= counts.logical_bytes);
    let vhdx = object_at(&f, "five.vhdx").unwrap();
    assert!(vhdx.allocated >= 5000, "{}", vhdx.allocated);
    #[cfg(windows)]
    assert_eq!(vhdx.allocated % 512, 0, "{}", vhdx.allocated);
    assert_eq!(
        counts.content_excluded, 2,
        "five.vhdx (disk image) and three.idb (database)"
    );
    assert_eq!(counts.content_pending, 5);

    // Subtree aggregates for `a`.
    let a = object_at(&f, "a").unwrap();
    let agg = f.catalog.directory_aggregate(a.id).unwrap().unwrap();
    assert_eq!(agg.file_count, 4);
    assert_eq!(agg.dir_count, 3);
    assert_eq!(agg.logical_bytes, 650);
    assert!(agg.complete);
    let ext = f.catalog.extension_counts(a.id, 10).unwrap();
    let get = |e: &str| {
        ext.iter()
            .find(|x| x.extension == e)
            .map(|x| x.count)
            .unwrap_or(0)
    };
    assert_eq!(get("txt"), 2);
    assert_eq!(get("cs"), 1);
    assert_eq!(get("idb"), 1);

    // Descendant predicate foundation (Q-4): `a` has both .idb and .cs even
    // though they live in a nested folder, while `e` has neither.
    let e = object_at(&f, "e").unwrap();
    let ext_e = f.catalog.extension_counts(e.id, 10).unwrap();
    assert!(ext_e.iter().all(|x| x.extension == "md"));

    // Completeness contract.
    let c = f.catalog.source_completeness(f.source).unwrap();
    assert!(c.metadata_complete);
    assert!(!c.content_complete);
    assert_eq!(c.content_pending, 5);

    // Path rendering. A rendered path is what the content pipeline opens, so
    // the contract is that it names the file on this filesystem - not that it
    // is spelled with any particular separator.
    let three = object_at(&f, "a/b/c/three.cs").unwrap();
    let path = f.catalog.render_path(three.id).unwrap().unwrap();
    assert_eq!(
        path,
        f.root
            .join("a")
            .join("b")
            .join("c")
            .join("three.cs")
            .display()
            .to_string()
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "a rendered path must be openable: {path}"
    );
    assert_eq!(three.kind, ObjectKind::File);
    assert_eq!(three.size, 300);
    assert_eq!(three.content_state, ContentState::Pending);
    let vhdx = object_at(&f, "five.vhdx").unwrap();
    assert_eq!(vhdx.content_state, ContentState::Excluded);
    let decisions = f.catalog.policy_decisions(vhdx.id).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].reason, "vm_disk_image");
}

#[test]
fn rescan_is_idempotent() {
    let f = fixture();
    scan(&f);
    let before = f.catalog.source_counts(f.source).unwrap();
    let s2 = scan(&f);
    assert_eq!(s2.generation, 2);
    assert_eq!(s2.stats.objects_created, 0);
    assert_eq!(s2.stats.entries_created, 0);
    assert_eq!(s2.tombstoned_entries + s2.tombstoned_objects, 0);
    let after = f.catalog.source_counts(f.source).unwrap();
    assert_eq!(before, after);
}

#[test]
fn rename_keeps_identity_and_generation() {
    let f = fixture();
    scan(&f);
    let before = object_at(&f, "a/one.txt").unwrap();
    std::fs::rename(f.root.join("a/one.txt"), f.root.join("a/uno.txt")).unwrap();
    let s = scan(&f);
    assert!(object_at(&f, "a/one.txt").is_none());
    let after = object_at(&f, "a/uno.txt").unwrap();
    assert_eq!(before.id, after.id, "native identity survives rename");
    assert_eq!(
        before.generation, after.generation,
        "no content re-extraction on rename"
    );
    assert_eq!(s.tombstoned_entries, 1);
    assert_eq!(s.tombstoned_objects, 0);
    let entries = f.catalog.entries_for_object(after.id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "uno.txt");
}

#[test]
fn content_change_bumps_generation() {
    let f = fixture();
    scan(&f);
    let before = object_at(&f, "a/one.txt").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(f.root.join("a/one.txt"), vec![b'x'; 150]).unwrap();
    scan(&f);
    let after = object_at(&f, "a/one.txt").unwrap();
    assert_eq!(before.id, after.id);
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.size, 150);
}

#[test]
fn hard_links_share_one_object() {
    let f = fixture();
    std::fs::hard_link(f.root.join("a/one.txt"), f.root.join("e/one-link.txt")).unwrap();
    scan(&f);
    let a = object_at(&f, "a/one.txt").unwrap();
    let b = object_at(&f, "e/one-link.txt").unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.link_count, 2);
    let entries = f.catalog.entries_for_object(a.id).unwrap();
    assert_eq!(entries.len(), 2);
    // Deleting one link keeps the object alive.
    std::fs::remove_file(f.root.join("e/one-link.txt")).unwrap();
    let s = scan(&f);
    assert_eq!(s.tombstoned_entries, 1);
    assert_eq!(s.tombstoned_objects, 0);
    assert_eq!(object_at(&f, "a/one.txt").unwrap().link_count, 1);
}

#[test]
fn deletion_tombstones_and_cascades() {
    let f = fixture();
    scan(&f);
    let counts_before = f.catalog.source_counts(f.source).unwrap();
    std::fs::remove_dir_all(f.root.join("a/b")).unwrap();
    let s = scan(&f);
    // a/b (dir), a/b/two.txt, a/b/c (dir), a/b/c/three.cs, a/b/c/three.idb
    assert_eq!(s.tombstoned_objects, 5);
    assert!(object_at(&f, "a/b").is_none());
    assert!(object_at(&f, "a/b/c/three.cs").is_none());
    let counts = f.catalog.source_counts(f.source).unwrap();
    assert_eq!(counts.files, counts_before.files - 3);
    assert_eq!(counts.directories, counts_before.directories - 2);
    let a = object_at(&f, "a").unwrap();
    let agg = f.catalog.directory_aggregate(a.id).unwrap().unwrap();
    assert_eq!(agg.logical_bytes, 100);
    assert_eq!(agg.dir_count, 1);
}

#[test]
fn subtree_move_preserves_objects() {
    let f = fixture();
    scan(&f);
    let b_before = object_at(&f, "a/b").unwrap();
    let three_before = object_at(&f, "a/b/c/three.cs").unwrap();
    std::fs::rename(f.root.join("a/b"), f.root.join("e/b")).unwrap();
    let s = scan(&f);
    let b_after = object_at(&f, "e/b").unwrap();
    let three_after = object_at(&f, "e/b/c/three.cs").unwrap();
    assert_eq!(b_before.id, b_after.id);
    assert_eq!(three_before.id, three_after.id);
    assert_eq!(three_before.generation, three_after.generation);
    assert_eq!(s.tombstoned_objects, 0);
    assert_eq!(s.tombstoned_entries, 1, "only the old a/b entry");
    let path = f.catalog.render_path(three_after.id).unwrap().unwrap();
    assert_eq!(
        path,
        f.root
            .join("e")
            .join("b")
            .join("c")
            .join("three.cs")
            .display()
            .to_string()
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "a rendered path must be openable after a move: {path}"
    );
    let a = object_at(&f, "a").unwrap();
    let e = object_at(&f, "e").unwrap();
    let agg_a = f.catalog.directory_aggregate(a.id).unwrap().unwrap();
    let agg_e = f.catalog.directory_aggregate(e.id).unwrap().unwrap();
    assert_eq!(agg_a.logical_bytes, 100);
    assert_eq!(agg_e.logical_bytes, 400 + 200 + 300 + 50);
    assert_eq!(agg_e.dir_count, 2);
}

/// A lister that fails for one directory to simulate access denied.
struct FailingLister {
    inner: Box<dyn DirectoryLister>,
    fail_name: String,
}

impl DirectoryLister for FailingLister {
    fn list(&self, dir: &Path) -> Result<Vec<RawEntry>, ScanError> {
        if dir.file_name() == Some(OsStr::new(&self.fail_name)) {
            return Err(ScanError::new(
                ScanErrorKind::AccessDenied,
                5,
                "simulated",
                dir,
            ));
        }
        self.inner.list(dir)
    }
    fn volume_info(&self, root: &Path) -> Result<VolumeInfo, ScanError> {
        self.inner.volume_info(root)
    }
    fn stat(&self, path: &Path) -> Result<RawEntry, ScanError> {
        self.inner.stat(path)
    }
    fn name(&self) -> &'static str {
        "failing"
    }
}

#[test]
fn unlisted_directory_preserves_previous_children() {
    let f = fixture();
    scan(&f);
    let failing = FailingLister {
        inner: default_lister(),
        fail_name: "b".into(),
    };
    let s = run_scan(&f.catalog, f.source, &failing, &RunScanOptions::default()).unwrap();
    assert_eq!(s.stats.errors, 1);
    assert_eq!(
        s.tombstoned_objects, 0,
        "children of the unlisted dir are kept"
    );
    assert!(object_at(&f, "a/b/c/three.cs").is_some());
    let b = object_at(&f, "a/b").unwrap();
    let agg_b = f.catalog.directory_aggregate(b.id).unwrap().unwrap();
    assert!(!agg_b.complete);
    let a = object_at(&f, "a").unwrap();
    assert!(
        !f.catalog
            .directory_aggregate(a.id)
            .unwrap()
            .unwrap()
            .complete,
        "incompleteness propagates up"
    );
    let e = object_at(&f, "e").unwrap();
    assert!(
        f.catalog
            .directory_aggregate(e.id)
            .unwrap()
            .unwrap()
            .complete
    );
    let errors = f.catalog.list_errors(Some(f.source), false, 10).unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind, "accessdenied");
    let src = f.catalog.get_source(f.source).unwrap().unwrap();
    assert!(src.state_reason.unwrap().contains("1 directories"));
    // A later clean scan resolves the error.
    scan(&f);
    assert!(f
        .catalog
        .list_errors(Some(f.source), false, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn unlistable_root_aborts_instead_of_publishing_empty() {
    let f = fixture();
    scan(&f);
    let failing = FailingLister {
        inner: default_lister(),
        fail_name: "root".into(),
    };
    let err = run_scan(&f.catalog, f.source, &failing, &RunScanOptions::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("root directory could not be listed"),
        "{err}"
    );
    let src = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(
        src.published_generation,
        Some(1),
        "previous generation stays published"
    );
    assert_eq!(src.state, SourceState::Degraded);
    let counts = f.catalog.source_counts(f.source).unwrap();
    assert_eq!(counts.files, 7, "previous contents preserved");
    let c = f.catalog.source_completeness(f.source).unwrap();
    assert!(
        !c.metadata_complete,
        "degraded sources are not reported complete"
    );

    // Listing errors below the root are reported in completeness.
    let partial = FailingLister {
        inner: default_lister(),
        fail_name: "b".into(),
    };
    run_scan(&f.catalog, f.source, &partial, &RunScanOptions::default()).unwrap();
    let c = f.catalog.source_completeness(f.source).unwrap();
    assert!(c.metadata_complete);
    assert_eq!(c.listing_errors, 1);
}

#[test]
fn interrupted_enumeration_never_publishes() {
    let f = fixture();
    let lister = default_lister();
    let mut session = f.catalog.begin_scan(f.source, ScanKind::Full).unwrap();
    // Ingest only the root event, then drop the session without finishing
    // (simulates a crash mid-scan).
    let mut first = None;
    eidos_scanner::walk(
        &f.root,
        lister.as_ref(),
        &eidos_scanner::WalkOptions {
            threads: 1,
            max_depth: Some(0),
            ..Default::default()
        },
        |ev| {
            if first.is_none() {
                first = Some(ev);
            }
        },
    );
    session.ingest(first.unwrap()).unwrap();
    session.commit().unwrap();
    // A real process crash skips Drop; leak the handle to leave the durable
    // generation open for startup recovery.
    std::mem::forget(session);

    let src = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(src.published_generation, None);
    assert_eq!(src.state, SourceState::Enumerating);
    let c = f.catalog.source_completeness(f.source).unwrap();
    assert!(!c.metadata_complete);

    // Startup recovery marks it aborted and leaves the source degraded so the
    // reconciler can distinguish an interrupted first scan from a deliberately
    // unscanned source.
    let report = f.catalog.recover().unwrap();
    assert_eq!(report.aborted_generations, vec![(f.source, 1)]);
    let src = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(src.published_generation, None);
    assert_eq!(src.state, SourceState::Degraded);
    let gens = f.catalog.list_generations(f.source, 10).unwrap();
    assert_eq!(gens[0].state, "aborted");

    // Partial rows are visible but the source says so; a fresh scan publishes.
    let s = scan(&f);
    assert_eq!(s.generation, 2);
    assert!(s.published);
    assert_eq!(
        f.catalog
            .get_source(f.source)
            .unwrap()
            .unwrap()
            .published_generation,
        Some(2)
    );
}

#[test]
fn dropping_a_scan_aborts_it_immediately() {
    let f = fixture();
    let session = f.catalog.begin_scan(f.source, ScanKind::Full).unwrap();

    drop(session);

    assert_eq!(f.catalog.open_scan_generation(f.source).unwrap(), None);
    let source = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(source.published_generation, None);
    assert_eq!(source.state, SourceState::New);
    let generations = f.catalog.list_generations(f.source, 10).unwrap();
    assert_eq!(generations[0].state, "aborted");
    assert!(generations[0]
        .note
        .as_deref()
        .unwrap_or_default()
        .contains("dropped before publication"));
}

#[test]
fn ingest_error_rolls_back_and_aborts_on_drop() {
    let f = fixture();
    let mut session = f.catalog.begin_scan(f.source, ScanKind::Full).unwrap();
    let invalid = DirEvent {
        token: DirToken(99),
        parent: Some(DirToken(0)),
        path: f.root.join("missing-parent-token"),
        depth: 1,
        result: Ok(vec![]),
        child_tokens: vec![],
    };

    let error = session.ingest(invalid).unwrap_err();
    assert!(error.to_string().contains("delivered before its parent"));
    drop(session);

    assert_eq!(f.catalog.open_scan_generation(f.source).unwrap(), None);
    let source = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(source.published_generation, None);
    assert_eq!(source.state, SourceState::New);
    let generation = &f.catalog.list_generations(f.source, 1).unwrap()[0];
    assert_eq!(generation.state, "aborted");
    assert!(generation
        .note
        .as_deref()
        .unwrap_or_default()
        .contains("dropped before publication"));
    assert_eq!(
        f.catalog.source_counts(f.source).unwrap().objects,
        1,
        "the source root is durable, but the failed batch was rolled back"
    );
}

#[test]
fn cancelled_walk_aborts_without_publishing() {
    let f = fixture();
    let cancel = Arc::new(AtomicBool::new(true));
    let options = RunScanOptions {
        walk: eidos_scanner::WalkOptions {
            cancel: Some(cancel),
            ..Default::default()
        },
        ..Default::default()
    };

    let error = run_scan(&f.catalog, f.source, default_lister().as_ref(), &options).unwrap_err();

    assert!(error.to_string().contains("scan cancelled"));
    let source = f.catalog.get_source(f.source).unwrap().unwrap();
    assert_eq!(source.published_generation, None);
    assert_eq!(source.state, SourceState::New);
    let generation = &f.catalog.list_generations(f.source, 1).unwrap()[0];
    assert_eq!(generation.state, "aborted");
    assert!(generation
        .note
        .as_deref()
        .unwrap_or_default()
        .contains("walk cancelled"));
}

#[test]
fn wide_scan_yields_to_catalog_writers() {
    const FILES: usize = 4_000;

    let f = fixture();
    let entries = (0..FILES)
        .map(|i| RawEntry {
            name: format!("wide-{i:05}.txt"),
            name_lossy: false,
            kind: ObjectKind::File,
            attributes: FileAttributes::default(),
            size: i as u64,
            allocated: Some(i as u64),
            created: None,
            modified: None,
            changed: None,
            accessed: None,
            native_id: None,
            reparse_tag: 0,
        })
        .collect();
    let event = DirEvent {
        token: DirToken(0),
        parent: None,
        path: f.root.clone(),
        depth: 0,
        result: Ok(entries),
        child_tokens: vec![],
    };
    let mut session = f.catalog.begin_scan(f.source, ScanKind::Full).unwrap();
    session.set_batching(256, Duration::from_millis(25));
    let acquisitions_before = f.catalog.writer_stats().acquisitions;
    let finished = Arc::new(AtomicBool::new(false));
    let scan_finished = finished.clone();

    let scan = std::thread::spawn(move || {
        session.ingest(event).unwrap();
        let summary = session.finish().unwrap();
        scan_finished.store(true, Ordering::Release);
        summary
    });

    let acquisition_deadline = Instant::now() + Duration::from_secs(5);
    while f.catalog.writer_stats().acquisitions == acquisitions_before {
        assert!(
            Instant::now() < acquisition_deadline,
            "scan did not acquire the writer gate"
        );
        std::thread::yield_now();
    }

    let mut writes = 0;
    let mut slowest_write = Duration::ZERO;
    while !finished.load(Ordering::Acquire) {
        let started = Instant::now();
        f.catalog
            .set_source_kind(f.source, SourceKind::WindowsGeneric)
            .unwrap();
        slowest_write = slowest_write.max(started.elapsed());
        writes += 1;
    }
    let summary = scan.join().unwrap();

    assert!(summary.published);
    assert_eq!(summary.stats.entries_seen, FILES as u64);
    assert!(
        writes >= 3,
        "only {writes} writes completed during the scan"
    );
    assert!(
        slowest_write < Duration::from_secs(5),
        "catalog writer stalled for {slowest_write:?}"
    );
    let writer_stats = f.catalog.writer_stats();
    assert!(writer_stats.acquisitions >= writes + 2);
    assert!(writer_stats.max_wait_ms > 0.0);
}

#[test]
fn second_open_generation_is_rejected() {
    let f = fixture();
    let s1 = f.catalog.begin_scan(f.source, ScanKind::Full).unwrap();
    let err = f
        .catalog
        .begin_scan(f.source, ScanKind::Full)
        .err()
        .unwrap();
    assert!(err.to_string().contains("open scan generation"));
    s1.abort("test").unwrap();
    assert!(f.catalog.begin_scan(f.source, ScanKind::Full).is_ok());
}

#[test]
fn children_listing_sorts_and_pages() {
    let f = fixture();
    scan(&f);
    let src = f.catalog.get_source(f.source).unwrap().unwrap();
    let root = src.root_object_id.unwrap();
    let page = f
        .catalog
        .list_children(
            root,
            &ChildrenPage {
                sort: ChildSort::Name,
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page.total, 4, "a, e, five.vhdx, empty.txt");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0].entry.name, "a");
    assert_eq!(page.rows[1].entry.name, "e");
    assert!(page.rows[0].aggregate.is_some());
    let page2 = f
        .catalog
        .list_children(
            root,
            &ChildrenPage {
                sort: ChildSort::Size,
                descending: true,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page2.rows[0].entry.name, "five.vhdx");
    assert_eq!(page2.rows.last().unwrap().entry.name, "empty.txt");
}

#[test]
fn progressive_visibility_during_scan() {
    let f = fixture();
    let lister = default_lister();
    let mut session = f.catalog.begin_scan(f.source, ScanKind::Full).unwrap();
    session.set_batching(1, std::time::Duration::from_secs(60));
    let mut seen = 0;
    let mut visible_mid_scan = None;
    eidos_scanner::walk(
        &f.root,
        lister.as_ref(),
        &eidos_scanner::WalkOptions {
            threads: 1,
            ..Default::default()
        },
        |ev| {
            session.ingest(ev).unwrap();
            seen += 1;
            if seen == 2 && visible_mid_scan.is_none() {
                // Another connection can already see committed rows.
                visible_mid_scan = Some(f.catalog.source_counts(f.source).unwrap().objects);
            }
        },
    );
    assert!(visible_mid_scan.unwrap() > 1);
    let summary = session.finish().unwrap();
    assert!(summary.published);
}
