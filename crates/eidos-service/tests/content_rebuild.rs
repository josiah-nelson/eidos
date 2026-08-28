//! Content-index rebuilds from stored chunks are serialised with workers,
//! commits, and search readiness: the rebuild is scheduled before anything
//! is advertised, owns the index writer while it runs, and leaves a
//! durable, recoverable state when it fails or is interrupted.

#![cfg(windows)]

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{SearchRequest, SourceId, SourceKind};
use eidos_search::content::{RebuildPhase, REBUILD_MARKER};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_service::content_workers::{spawn_content_workers, top_up_queue};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const NOTES: usize = 30;

// Each case opens a Tantivy writer with its own worker pool and then starts
// the service's background threads. Running all five at once is needlessly
// hostile to the smaller Windows runner and can turn the teardown deadline
// into a cascade of false failures. The production concurrency contract is
// exercised inside each case; only the independent fixtures are serialised.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn serial_fixture() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    sid: SourceId,
    chunks: u64,
}

fn open_state(data: &Path) -> Arc<AppState> {
    let cfg = ServiceConfig {
        data_dir: data.to_path_buf(),
        scan_threads: 2,
        auto_reconcile: false,
        content_workers: 2,
        ..Default::default()
    };
    Arc::new(AppState::open(&cfg).unwrap())
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn search(state: &AppState, q: &str) -> eidos_domain::SearchResponse {
    let parsed = eidos_query::parse(q).unwrap();
    let r = SearchRequest::new(parsed.query);
    search_with_content(
        &state.index,
        Some(&state.content_index),
        &state.catalog,
        &r,
        &ExecOptions::default(),
    )
    .unwrap()
}

fn content_dir(data: &Path) -> PathBuf {
    data.join("index").join("content")
}

fn scan(state: &AppState, sid: SourceId) {
    run_scan(
        &state.catalog,
        sid,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    eidos_service::follower::follow_once(state).unwrap();
}

/// A fully indexed source, then the service is stopped.
fn indexed_env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    for i in 0..NOTES {
        std::fs::write(
            root.join(format!("docs/note{i}.md")),
            format!("note {i}\nshared marker alpha\nunique token omega{i}\n"),
        )
        .unwrap();
    }
    let data = dir.path().join("data");
    let state = open_state(&data);
    let sid = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "fixture".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    scan(&state, sid);
    spawn_content_workers(&state, 2);
    top_up_queue(&state).unwrap();
    assert!(
        wait_until(Duration::from_secs(30), || {
            state
                .catalog
                .source_completeness(sid)
                .map(|c| c.content_complete)
                .unwrap_or(false)
        }),
        "content never completed"
    );
    let chunks = state.catalog.content_stats(None).unwrap().chunks;
    assert_eq!(chunks, NOTES as u64);
    assert_eq!(state.content_index.num_docs(), chunks);
    stop(state);
    Env {
        _dir: dir,
        root,
        data,
        sid,
        chunks,
    }
}

/// Stop the service and wait for every background thread to release its
/// handle, so the next `AppState::open` can take the index writer lock.
fn stop(state: Arc<AppState>) {
    state.request_shutdown();
    assert!(
        wait_until(Duration::from_secs(10), || Arc::strong_count(&state) == 1),
        "background threads did not exit"
    );
    drop(state);
}

fn lose_index(data: &Path) {
    std::fs::remove_dir_all(content_dir(data)).unwrap();
}

#[test]
fn lost_index_is_rebuilt_before_readiness_and_restart_is_clean() {
    let _serial = serial_fixture();
    let e = indexed_env();
    lose_index(&e.data);

    let state = open_state(&e.data);
    // Scheduled synchronously at open: nothing has been advertised yet.
    assert!(state.content_rebuild);
    let st = state.content_index.rebuild_status();
    assert_eq!(st.phase, RebuildPhase::Pending);
    assert_eq!(st.chunks, e.chunks);
    assert!(content_dir(&e.data).join(REBUILD_MARKER).exists());
    let r = search(&state, "content:alpha");
    assert_eq!(r.hits.len(), 0);
    assert!(!r.all_sources_complete(true));
    assert!(
        r.coverage
            .degraded
            .iter()
            .any(|d| d.detail.contains("being rebuilt")),
        "{:?}",
        r.coverage
    );
    assert!(!r.coverage.full);

    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(30), || !state
        .content_index
        .is_rebuilding()));
    let st = state.content_index.rebuild_status();
    assert_eq!(st.phase, RebuildPhase::Idle, "{st:?}");
    assert_eq!(st.docs, e.chunks);
    assert!(st.error.is_none());
    assert!(!content_dir(&e.data).join(REBUILD_MARKER).exists());
    let r = search(&state, "content:alpha");
    assert_eq!(r.total.value, NOTES as u64);
    assert!(r.all_sources_complete(true), "{:?}", r.warnings);
    assert_eq!(state.content_index.num_docs(), e.chunks);
    stop(state);

    // A clean restart does not rebuild again.
    let state = open_state(&e.data);
    assert!(!state.content_rebuild);
    assert_eq!(
        state.content_index.rebuild_status().phase,
        RebuildPhase::Idle
    );
    assert_eq!(state.content_index.num_docs(), e.chunks);
}

#[test]
fn workers_and_commits_wait_for_the_rebuild_then_resume() {
    let _serial = serial_fixture();
    let e = indexed_env();
    lose_index(&e.data);
    // A new file is queued for extraction while the rebuild is pending.
    std::fs::write(e.root.join("docs/late.md"), b"late arrival with kappa\n").unwrap();

    let state = open_state(&e.data);
    assert!(state.content_index.is_rebuilding());
    scan(&state, e.sid);
    top_up_queue(&state).unwrap();
    assert!(state.catalog.job_counts(None).unwrap().queued >= 1);

    // Hold the rebuild on its first document until released.
    let release = Arc::new(AtomicBool::new(false));
    let rel = release.clone();
    state
        .content_index
        .set_rebuild_pacer(Some(Arc::new(move |_| {
            while !rel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        })));
    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(10), || {
        state.content_index.rebuild_status().phase == RebuildPhase::Running
    }));
    std::thread::sleep(Duration::from_millis(1200));
    // Workers claimed nothing and the coordinator committed nothing.
    let view = state
        .content_workers
        .view(state.content_index.uncommitted());
    assert_eq!(view.files_indexed, 0, "{view:?}");
    assert_eq!(view.commits, 0, "{view:?}");
    assert_eq!(state.content_index.uncommitted(), 0);
    assert!(state.catalog.job_counts(None).unwrap().queued >= 1);
    assert_eq!(state.catalog.job_counts(None).unwrap().running, 0);
    let r = search(&state, "content:kappa");
    assert!(!r.all_sources_complete(true));

    release.store(true, Ordering::Relaxed);
    assert!(wait_until(Duration::from_secs(30), || !state
        .content_index
        .is_rebuilding()));
    assert_eq!(
        state.content_index.rebuild_status().phase,
        RebuildPhase::Idle
    );
    // The queued job is now processed and published.
    assert!(
        wait_until(Duration::from_secs(30), || {
            state
                .catalog
                .source_completeness(e.sid)
                .map(|c| c.content_complete)
                .unwrap_or(false)
        }),
        "late file never indexed: {:?}",
        state.content_workers.view(0)
    );
    assert_eq!(state.content_workers.view(0).files_indexed, 1);
    assert_eq!(search(&state, "content:kappa").hits.len(), 1);
    assert_eq!(search(&state, "content:alpha").total.value, NOTES as u64);
    assert_eq!(state.content_index.num_docs(), e.chunks + 1);
    stop(state);
}

#[test]
fn rebuild_failure_is_visible_and_retried_at_the_next_start() {
    let _serial = serial_fixture();
    let e = indexed_env();
    lose_index(&e.data);

    let state = open_state(&e.data);
    state.content_index.set_rebuild_pacer(Some(Arc::new(|docs| {
        if docs == 5 {
            Err("disk full".into())
        } else {
            Ok(())
        }
    })));
    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(10), || {
        state.content_index.rebuild_status().phase == RebuildPhase::Failed
    }));
    let st = state.content_index.rebuild_status();
    assert!(st.error.as_deref().unwrap().contains("disk full"), "{st:?}");
    assert_eq!(st.docs, 5);
    assert!(
        content_dir(&e.data).join(REBUILD_MARKER).exists(),
        "marker kept so the next start rebuilds"
    );
    assert!(!state.content_index.is_rebuilding());
    let r = search(&state, "content:alpha");
    assert!(!r.all_sources_complete(true));
    assert!(
        r.coverage
            .degraded
            .iter()
            .any(|d| d.detail.contains("rebuild failed") && d.detail.contains("disk full")),
        "{:?}",
        r.coverage
    );
    stop(state);

    // Restart: the marker schedules the rebuild again even though the index
    // is no longer empty; this time it completes.
    let state = open_state(&e.data);
    assert!(state.content_rebuild);
    let st = state.content_index.rebuild_status();
    assert_eq!(st.phase, RebuildPhase::Pending);
    assert!(!search(&state, "content:alpha").all_sources_complete(true));
    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(30), || !state
        .content_index
        .is_rebuilding()));
    assert_eq!(
        state.content_index.rebuild_status().phase,
        RebuildPhase::Idle
    );
    assert!(!content_dir(&e.data).join(REBUILD_MARKER).exists());
    assert_eq!(state.content_index.num_docs(), e.chunks);
    assert!(search(&state, "content:alpha").all_sources_complete(true));
    stop(state);
}

#[test]
fn shutdown_during_rebuild_leaves_it_pending_for_the_next_start() {
    let _serial = serial_fixture();
    let e = indexed_env();
    lose_index(&e.data);

    let state = open_state(&e.data);
    let release = Arc::new(AtomicBool::new(false));
    let rel = release.clone();
    state
        .content_index
        .set_rebuild_pacer(Some(Arc::new(move |_| {
            while !rel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        })));
    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(10), || {
        state.content_index.rebuild_status().phase == RebuildPhase::Running
    }));
    state.request_shutdown();
    release.store(true, Ordering::Relaxed);
    assert!(wait_until(Duration::from_secs(10), || {
        state.content_index.rebuild_status().phase == RebuildPhase::Failed
    }));
    let st = state.content_index.rebuild_status();
    assert!(st.error.as_deref().unwrap().contains("shutdown"), "{st:?}");
    assert!(content_dir(&e.data).join(REBUILD_MARKER).exists());
    stop(state);

    let state = open_state(&e.data);
    assert!(state.content_rebuild);
    assert_eq!(
        state.content_index.rebuild_status().phase,
        RebuildPhase::Pending
    );
    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(30), || !state
        .content_index
        .is_rebuilding()));
    assert_eq!(
        state.content_index.rebuild_status().phase,
        RebuildPhase::Idle
    );
    assert_eq!(state.content_index.num_docs(), e.chunks);
    stop(state);
}

#[test]
fn torn_marker_still_schedules_a_rebuild() {
    let _serial = serial_fixture();
    let e = indexed_env();
    // The index is intact, but a crash left a truncated marker behind.
    std::fs::write(content_dir(&e.data).join(REBUILD_MARKER), b"{\"chunks\": 3").unwrap();

    let state = open_state(&e.data);
    assert!(
        state.content_rebuild,
        "marker presence alone schedules a rebuild"
    );
    let st = state.content_index.rebuild_status();
    assert_eq!(st.phase, RebuildPhase::Pending);
    assert_eq!(
        st.chunks, e.chunks,
        "chunk count refreshed from the catalog"
    );
    assert!(!search(&state, "content:alpha").all_sources_complete(true));
    state.start_background().unwrap();
    assert!(wait_until(Duration::from_secs(30), || !state
        .content_index
        .is_rebuilding()));
    assert_eq!(
        state.content_index.rebuild_status().phase,
        RebuildPhase::Idle
    );
    assert!(!content_dir(&e.data).join(REBUILD_MARKER).exists());
    assert_eq!(state.content_index.num_docs(), e.chunks);
    assert!(search(&state, "content:alpha").all_sources_complete(true));
    stop(state);
}
