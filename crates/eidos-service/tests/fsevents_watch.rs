//! End-to-end macOS change feed: publish with a cursor, apply live FSEvents,
//! resume from the stored cursor after a restart, and reconcile when the feed
//! says its history is incomplete.
//!
//! The fixture lives on the boot volume's temporary directory, which keeps an
//! event store. A volume without one (a read-only volume, for instance)
//! cannot issue a cursor at all; that path is covered by the unit tests.

#![cfg(target_os = "macos")]

use eidos_catalog::NewSource;
use eidos_domain::{SourceId, SourceKind, SourceState};
use eidos_service::scanner::{start_scan, wait_for_scan};
use eidos_service::state::AppState;
use eidos_service::watcher::{ensure_watcher, FsEventsCheckpoint, WatcherFeed, WatcherState};
use eidos_service::ServiceConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// FSEvents coalesces over half a second and the watcher polls on top of
/// that, so "visible" means visible within a few windows, not instantly.
const VISIBLE: Duration = Duration::from_secs(15);

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    // FSEvents reports resolved paths, and the temporary directory is behind
    // a symlink on macOS; the source root has to agree with what arrives.
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("src");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/readme.md"), b"hello").unwrap();
    std::fs::write(root.join("a.txt"), b"aaaa").unwrap();
    Env {
        data: base.join("data"),
        _dir: dir,
        root,
    }
}

fn open_state(data: &Path) -> Arc<AppState> {
    let cfg = ServiceConfig {
        data_dir: data.to_path_buf(),
        scan_threads: 2,
        auto_reconcile: false,
        ..Default::default()
    };
    Arc::new(AppState::open(&cfg).unwrap())
}

fn add_source(state: &AppState, root: &Path) -> SourceId {
    state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "fsevents-fixture".into(),
            kind: SourceKind::MacosLocal,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap()
}

/// Stop the agent the way a service shutdown does: cancel the watcher, wait
/// for its thread to release the state, then drop the last handle. Without
/// this the search index stays locked by a live watcher thread.
fn stop_agent(
    state: Arc<AppState>,
    sid: SourceId,
    watcher: &Arc<eidos_service::watcher::WatcherStatus>,
) {
    state
        .shutdown
        .store(true, std::sync::atomic::Ordering::Release);
    eidos_service::watcher::stop_watcher(&state, sid);
    assert!(
        wait_until(Duration::from_secs(30), || watcher.is_stopped()).is_some(),
        "watcher did not stop"
    );
    drop(state);
    // The watcher thread sets its state just before returning; give it a
    // moment to drop its own handle on the way out.
    std::thread::sleep(Duration::from_millis(200));
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn exists(state: &AppState, sid: SourceId, rel: &str) -> bool {
    state.catalog.resolve_relative(sid, rel).unwrap().is_some()
}

fn id_of(state: &AppState, sid: SourceId, rel: &str) -> eidos_domain::ObjectId {
    state
        .catalog
        .resolve_relative(sid, rel)
        .unwrap()
        .unwrap_or_else(|| panic!("{rel} should be catalogued"))
}

fn aggregate(state: &AppState, sid: SourceId, rel: &str) -> eidos_catalog::DirectoryAggregate {
    let id = if rel.is_empty() {
        state
            .catalog
            .get_source(sid)
            .unwrap()
            .unwrap()
            .root_object_id
            .unwrap()
    } else {
        id_of(state, sid, rel)
    };
    state.catalog.directory_aggregate(id).unwrap().unwrap()
}

/// A volume with no event store cannot issue a cursor, so there is nothing
/// for these tests to assert. That is a real configuration (a read-only
/// volume, or a runner whose temporary filesystem keeps no history), not a
/// failure, so the test says so and passes.
fn has_event_store(root: &Path) -> bool {
    if eidos_scanner::fsevents::store_uuid(root).is_some() {
        return true;
    }
    eprintln!("skipping: {} keeps no FSEvents history", root.display());
    false
}

/// Scan, then wait for the watcher the scan sequence starts.
fn scan_and_watch(
    state: &Arc<AppState>,
    sid: SourceId,
) -> Arc<eidos_service::watcher::WatcherStatus> {
    let progress = start_scan(state, sid).unwrap();
    let summary = wait_for_scan(&progress, Duration::from_secs(60))
        .expect("scan finished")
        .unwrap();
    assert!(summary.published, "the scan must publish a generation");
    let watcher = ensure_watcher(state, sid);
    assert!(
        wait_until(VISIBLE, || watcher.view().state == WatcherState::Live).is_some(),
        "watcher never went live: {:?}",
        watcher.view()
    );
    watcher
}

#[test]
fn a_published_scan_carries_a_validated_cursor() {
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    scan_and_watch(&state, sid);

    let (checkpoint, _) = state
        .catalog
        .checkpoint(sid)
        .unwrap()
        .expect("an fsevents checkpoint");
    let checkpoint = FsEventsCheckpoint::from_checkpoint(&checkpoint).expect("fsevents kind");
    assert!(checkpoint.cursor.event_id > 0);
    assert_eq!(
        checkpoint.cursor.store_uuid.len(),
        36,
        "the event store's identity travels with the id"
    );
    assert_eq!(
        eidos_scanner::fsevents::store_uuid(&e.root).as_deref(),
        Some(checkpoint.cursor.store_uuid.as_str()),
        "the cursor must name the store it came from"
    );
}

#[test]
fn a_source_with_a_cursor_reports_a_live_feed() {
    // Freshness and the reconciler both used to ask "is the checkpoint a USN
    // journal?", which reports every macOS source as periodic and rescans it
    // on a timer even while its feed is applying every change.
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    scan_and_watch(&state, sid);

    let completeness = state.catalog.source_completeness(sid).unwrap();
    assert_eq!(
        completeness.freshness,
        eidos_domain::Freshness::Live,
        "an FSEvents cursor is a live feed: {completeness:?}"
    );
}

#[test]
fn live_changes_reach_the_catalog() {
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    let watcher = scan_and_watch(&state, sid);
    assert_eq!(watcher.view().feed, WatcherFeed::MacosFsEvents);

    // Create.
    std::fs::write(e.root.join("docs/new.txt"), b"new").unwrap();
    wait_until(VISIBLE, || exists(&state, sid, "docs/new.txt")).expect("create visible");
    assert_eq!(aggregate(&state, sid, "docs").file_count, 2);

    // Modify: same object, later generation.
    let id = id_of(&state, sid, "docs/new.txt");
    let before = state.catalog.get_object(id).unwrap().unwrap();
    std::fs::write(e.root.join("docs/new.txt"), b"new content that is longer").unwrap();
    wait_until(VISIBLE, || {
        state.catalog.get_object(id).unwrap().unwrap().size == 26
    })
    .expect("modify visible");
    let after = state.catalog.get_object(id).unwrap().unwrap();
    assert!(after.generation > before.generation);

    // Rename inside one directory keeps the object.
    std::fs::rename(e.root.join("docs/new.txt"), e.root.join("docs/renamed.txt")).unwrap();
    wait_until(VISIBLE, || {
        exists(&state, sid, "docs/renamed.txt") && !exists(&state, sid, "docs/new.txt")
    })
    .expect("rename visible");
    assert_eq!(
        state
            .catalog
            .resolve_relative(sid, "docs/renamed.txt")
            .unwrap(),
        Some(id),
        "a rename must not replace the object"
    );

    // A hard link is a second entry for one object.
    std::fs::hard_link(e.root.join("a.txt"), e.root.join("docs/a-link.txt")).unwrap();
    wait_until(VISIBLE, || exists(&state, sid, "docs/a-link.txt")).expect("hard link visible");
    assert_eq!(
        state
            .catalog
            .resolve_relative(sid, "docs/a-link.txt")
            .unwrap(),
        Some(id_of(&state, sid, "a.txt"))
    );

    // A backslash is an ordinary macOS file-name character. The catalog must
    // not split it into two components when applying or resolving an event.
    let odd = r"odd\name.txt";
    std::fs::write(e.root.join(odd), b"odd").unwrap();
    wait_until(VISIBLE, || exists(&state, sid, odd)).expect("backslash-name create visible");
    std::fs::remove_file(e.root.join(odd)).unwrap();
    wait_until(VISIBLE, || !exists(&state, sid, odd)).expect("backslash-name delete visible");

    // Deleting a directory takes its subtree with it.
    std::fs::create_dir_all(e.root.join("pkg/inner")).unwrap();
    std::fs::write(e.root.join("pkg/inner/x.rs"), b"fn x() {}").unwrap();
    wait_until(VISIBLE, || exists(&state, sid, "pkg/inner/x.rs")).expect("nested create visible");
    std::fs::remove_dir_all(e.root.join("pkg")).unwrap();
    wait_until(VISIBLE, || !exists(&state, sid, "pkg")).expect("delete visible");
    assert!(!exists(&state, sid, "pkg/inner/x.rs"));
}

#[test]
fn a_rename_that_only_changes_case_replaces_the_entry() {
    // On a case-insensitive volume the old path still resolves after
    // `mv Report.txt report.txt` - to the same file - so "does the old path
    // exist?" is the wrong question. Asking it leaves the catalog holding
    // both spellings of one object.
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    scan_and_watch(&state, sid);

    std::fs::write(e.root.join("Report.txt"), b"one").unwrap();
    wait_until(VISIBLE, || exists(&state, sid, "Report.txt")).expect("create visible");
    let id = id_of(&state, sid, "Report.txt");

    std::fs::rename(e.root.join("Report.txt"), e.root.join("report.txt")).unwrap();
    wait_until(VISIBLE, || {
        state
            .catalog
            .entries_for_object(id)
            .unwrap()
            .iter()
            .any(|entry| entry.name == "report.txt")
    })
    .expect("the new spelling must be catalogued");
    let names: Vec<String> = state
        .catalog
        .entries_for_object(id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        names,
        vec!["report.txt".to_string()],
        "one file must not be catalogued under two spellings"
    );
}

#[test]
fn a_subtree_moved_in_is_enumerated_not_guessed() {
    // Nothing inside a moved directory generates a notification of its own,
    // so the translator has to read the subtree it just learned about.
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    scan_and_watch(&state, sid);

    let outside = e.root.parent().unwrap().join("staged");
    std::fs::create_dir_all(outside.join("deep/deeper")).unwrap();
    std::fs::write(outside.join("deep/one.txt"), b"one").unwrap();
    std::fs::write(outside.join("deep/deeper/two.txt"), b"two").unwrap();
    std::fs::rename(&outside, e.root.join("staged")).unwrap();

    wait_until(VISIBLE, || {
        exists(&state, sid, "staged/deep/deeper/two.txt")
    })
    .expect("the moved subtree must be enumerated");
    assert!(exists(&state, sid, "staged/deep/one.txt"));
    assert_eq!(aggregate(&state, sid, "staged").file_count, 2);
}

#[test]
fn a_directory_move_keeps_every_identity_beneath_it() {
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    scan_and_watch(&state, sid);

    std::fs::create_dir_all(e.root.join("pkg/inner")).unwrap();
    std::fs::write(e.root.join("pkg/inner/x.rs"), b"fn x() {}").unwrap();
    wait_until(VISIBLE, || exists(&state, sid, "pkg/inner/x.rs")).expect("nested create visible");
    let package = id_of(&state, sid, "pkg");
    let leaf = id_of(&state, sid, "pkg/inner/x.rs");

    std::fs::rename(e.root.join("pkg"), e.root.join("docs/pkg")).unwrap();
    wait_until(VISIBLE, || {
        exists(&state, sid, "docs/pkg/inner/x.rs") && !exists(&state, sid, "pkg")
    })
    .expect("move visible");
    assert_eq!(
        state.catalog.resolve_relative(sid, "docs/pkg").unwrap(),
        Some(package),
        "the moved directory keeps its identity"
    );
    assert_eq!(
        state
            .catalog
            .resolve_relative(sid, "docs/pkg/inner/x.rs")
            .unwrap(),
        Some(leaf),
        "a move must not re-create the subtree"
    );
}

#[test]
fn a_restart_resumes_from_the_stored_cursor() {
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    let watcher = scan_and_watch(&state, sid);
    let generation = state
        .catalog
        .get_source(sid)
        .unwrap()
        .unwrap()
        .published_generation;

    // Stop the agent, change the tree while it is not running, start again.
    stop_agent(state, sid, &watcher);
    std::fs::write(e.root.join("docs/offline.txt"), b"written while stopped").unwrap();
    std::fs::remove_file(e.root.join("a.txt")).unwrap();

    let state = open_state(&e.data);
    let watcher = ensure_watcher(&state, sid);
    assert!(
        wait_until(VISIBLE, || watcher.view().state == WatcherState::Live).is_some(),
        "watcher never went live: {:?}",
        watcher.view()
    );
    wait_until(VISIBLE, || {
        exists(&state, sid, "docs/offline.txt") && !exists(&state, sid, "a.txt")
    })
    .expect("stored history must be replayed");
    assert_eq!(
        state
            .catalog
            .get_source(sid)
            .unwrap()
            .unwrap()
            .published_generation,
        generation,
        "replaying history is not a new generation"
    );
}

#[test]
fn background_start_leaves_a_feedless_source_to_periodic_reconciliation() {
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    let watcher = scan_and_watch(&state, sid);
    stop_agent(state, sid, &watcher);

    // A no-history volume publishes a generation without a checkpoint. Clear
    // a real one here to reproduce that durable state on a normal APFS test
    // volume, then exercise the service-restart path.
    let state = open_state(&e.data);
    state.catalog.clear_checkpoint(sid).unwrap();
    state.start_background().unwrap();

    assert!(
        state.watcher_status(sid).is_none(),
        "a source without a resumable cursor belongs to the periodic reconciler"
    );
    state.request_shutdown();
}

#[test]
fn a_cursor_from_a_replaced_event_store_forces_reconciliation() {
    let e = env();
    if !has_event_store(&e.root) {
        return;
    }
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    let watcher = scan_and_watch(&state, sid);

    // A store is replaced while the agent is not running, so the mismatch is
    // discovered when the stream is opened, not mid-stream.
    stop_agent(state, sid, &watcher);
    let state = open_state(&e.data);
    let (checkpoint, _) = state.catalog.checkpoint(sid).unwrap().unwrap();
    let mut checkpoint = FsEventsCheckpoint::from_checkpoint(&checkpoint).unwrap();
    checkpoint.cursor.store_uuid = "00000000-0000-0000-0000-000000000000".into();
    state
        .catalog
        .set_checkpoint(sid, &checkpoint.to_checkpoint())
        .unwrap();

    let watcher = ensure_watcher(&state, sid);
    assert!(
        wait_until(VISIBLE, || {
            let view = watcher.view();
            view.state == WatcherState::Reconciling
                || state.catalog.get_source(sid).unwrap().unwrap().state == SourceState::Degraded
        })
        .is_some(),
        "an unusable cursor must degrade the source instead of resuming: {:?}",
        watcher.view()
    );
}
