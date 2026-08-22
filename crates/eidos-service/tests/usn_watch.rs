//! Milestone 2 end-to-end: native scan sequence, live USN watcher, restart
//! from checkpoint, and synthetic overflow → reconcile.
//!
//! Requires an elevated process with a USN-journaled temp volume; otherwise
//! the test logs a skip and passes.

#![cfg(windows)]

use eidos_catalog::NewSource;
use eidos_domain::{SourceId, SourceKind, SourceState};
use eidos_service::scanner::{start_scan, wait_for_scan};
use eidos_service::state::AppState;
use eidos_service::watcher::{ensure_watcher, UsnCheckpoint, WatcherState};
use eidos_service::ServiceConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn journal_available() -> bool {
    let tmp = std::env::temp_dir();
    let root = match tmp.to_string_lossy().get(0..3) {
        Some(r) => r.to_string(),
        None => return false,
    };
    match eidos_scanner::usn::VolumeHandle::open(&root) {
        Ok(v) => eidos_scanner::usn::query_journal(&v).is_ok(),
        Err(_) => false,
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/readme.md"), b"hello").unwrap();
    std::fs::write(root.join("a.txt"), b"aaaa").unwrap();
    Env {
        data: dir.path().join("data"),
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
            name: "usn-fixture".into(),
            kind: SourceKind::WindowsLocal,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap()
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

#[test]
fn live_changes_restart_and_overflow() {
    if !journal_available() {
        eprintln!("skipping: USN journal not readable (run elevated on an NTFS temp volume)");
        return;
    }
    let e = env();
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);

    // 1. Native scan sequence establishes a checkpoint and a watcher.
    let p = start_scan(&state, sid).unwrap();
    let summary = wait_for_scan(&p, Duration::from_secs(30))
        .expect("scan finished")
        .unwrap();
    assert!(summary.published);
    let (cp, _) = state
        .catalog
        .checkpoint(sid)
        .unwrap()
        .expect("usn checkpoint");
    let cp = UsnCheckpoint::from_checkpoint(&cp).unwrap();
    assert!(cp.next_usn > 0);
    let watcher = ensure_watcher(&state, sid);
    assert!(wait_until(Duration::from_secs(10), || watcher.view().state
        == WatcherState::Live)
    .is_some());

    // 2. Create: visible within the 2-second gate.
    std::fs::write(e.root.join("docs/new.txt"), b"new").unwrap();
    let latency = wait_until(Duration::from_secs(5), || {
        exists(&state, sid, "docs/new.txt")
    })
    .expect("create visible");
    eprintln!("create visible after {latency:?}");
    assert!(
        latency < Duration::from_secs(2),
        "visibility latency {latency:?}"
    );
    let agg = |rel: &str| {
        let id = if rel.is_empty() {
            state
                .catalog
                .get_source(sid)
                .unwrap()
                .unwrap()
                .root_object_id
                .unwrap()
        } else {
            state.catalog.resolve_relative(sid, rel).unwrap().unwrap()
        };
        state.catalog.directory_aggregate(id).unwrap().unwrap()
    };
    assert_eq!(agg("docs").file_count, 2);
    assert_eq!(agg("").file_count, 3);

    // 3. Modify: generation bump.
    let id = state
        .catalog
        .resolve_relative(sid, "docs/new.txt")
        .unwrap()
        .unwrap();
    let before = state.catalog.get_object(id).unwrap().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    std::fs::write(e.root.join("docs/new.txt"), b"new content that is longer").unwrap();
    wait_until(Duration::from_secs(5), || {
        state.catalog.get_object(id).unwrap().unwrap().size == 26
    })
    .expect("modify visible");
    let after = state.catalog.get_object(id).unwrap().unwrap();
    assert_eq!(after.generation, before.generation + 1);

    // 4. Rename keeps identity.
    std::fs::rename(e.root.join("docs/new.txt"), e.root.join("docs/renamed.txt")).unwrap();
    wait_until(Duration::from_secs(5), || {
        exists(&state, sid, "docs/renamed.txt") && !exists(&state, sid, "docs/new.txt")
    })
    .expect("rename visible");
    assert_eq!(
        state
            .catalog
            .resolve_relative(sid, "docs/renamed.txt")
            .unwrap(),
        Some(id)
    );
    assert_eq!(
        state.catalog.get_object(id).unwrap().unwrap().generation,
        after.generation
    );

    // 5. New directory with a file inside, then move the directory.
    std::fs::create_dir_all(e.root.join("pkg/inner")).unwrap();
    std::fs::write(e.root.join("pkg/inner/x.cs"), b"class X {}").unwrap();
    wait_until(Duration::from_secs(5), || {
        exists(&state, sid, "pkg/inner/x.cs")
    })
    .expect("nested create visible");
    assert_eq!(agg("pkg").file_count, 1);
    std::fs::rename(e.root.join("pkg"), e.root.join("docs/pkg")).unwrap();
    wait_until(Duration::from_secs(5), || {
        exists(&state, sid, "docs/pkg/inner/x.cs") && !exists(&state, sid, "pkg")
    })
    .expect("move visible");
    assert_eq!(agg("docs").dir_count, 2);
    assert_eq!(agg("docs").file_count, 3);
    assert_eq!(agg("").dir_count, 3);

    // 6. Hard link then delete.
    std::fs::hard_link(e.root.join("a.txt"), e.root.join("docs/a-link.txt")).unwrap();
    wait_until(Duration::from_secs(5), || {
        exists(&state, sid, "docs/a-link.txt")
    })
    .expect("link visible");
    let a_id = state
        .catalog
        .resolve_relative(sid, "a.txt")
        .unwrap()
        .unwrap();
    assert_eq!(
        state
            .catalog
            .resolve_relative(sid, "docs/a-link.txt")
            .unwrap(),
        Some(a_id)
    );
    std::fs::remove_dir_all(e.root.join("docs/pkg")).unwrap();
    wait_until(Duration::from_secs(5), || !exists(&state, sid, "docs/pkg"))
        .expect("delete visible");
    assert!(!exists(&state, sid, "docs/pkg/inner/x.cs"));
    assert_eq!(agg("docs").dir_count, 0);
    assert_eq!(
        agg("").file_count,
        4,
        "a.txt, readme.md, renamed.txt, a-link.txt"
    );
    let gens_before = state.catalog.list_generations(sid, 10).unwrap().len();
    assert_eq!(gens_before, 1, "no rescans were needed");

    // 7. Restart: changes made while stopped are caught up from the checkpoint.
    state.request_shutdown();
    assert!(wait_until(Duration::from_secs(5), || watcher.is_stopped()).is_some());
    drop(state);
    std::fs::write(e.root.join("offline.txt"), b"while stopped").unwrap();
    std::fs::remove_file(e.root.join("a.txt")).unwrap();
    let state = open_state(&e.data);
    state.start_background().unwrap();
    let t = wait_until(Duration::from_secs(10), || {
        exists(&state, sid, "offline.txt") && !exists(&state, sid, "a.txt")
    })
    .expect("catch-up after restart");
    eprintln!("catch-up after restart took {t:?}");
    assert_eq!(
        state.catalog.list_generations(sid, 10).unwrap().len(),
        1,
        "restart did not rebuild"
    );
    // a.txt had two links; only the second remains.
    assert_eq!(
        state
            .catalog
            .resolve_relative(sid, "docs/a-link.txt")
            .unwrap(),
        Some(a_id)
    );

    // 8. Synthetic overflow: point the checkpoint far into the past.
    let watcher = state.watcher_status(sid).unwrap();
    let (cp, _) = state.catalog.checkpoint(sid).unwrap().unwrap();
    let mut bad = UsnCheckpoint::from_checkpoint(&cp).unwrap();
    bad.journal_id ^= 0x1234_5678;
    state
        .catalog
        .set_checkpoint(sid, &bad.to_checkpoint())
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(15), || watcher.view().reconciles >= 1).is_some(),
        "reconcile triggered"
    );
    let ok = wait_until(Duration::from_secs(60), || {
        let s = state.catalog.get_source(sid).unwrap().unwrap();
        s.published_generation == Some(2)
            && matches!(
                s.state,
                SourceState::ContentPending | SourceState::MetadataComplete
            )
            && state.catalog.checkpoint(sid).unwrap().is_some()
    });
    assert!(
        ok.is_some(),
        "reconciliation published a new generation and re-established the checkpoint"
    );
    let gens = state.catalog.list_generations(sid, 10).unwrap();
    assert_eq!(gens.len(), 2);
    // Catalog still correct after reconcile.
    assert!(exists(&state, sid, "offline.txt"));
    assert!(!exists(&state, sid, "a.txt"));
    assert!(exists(&state, sid, "docs/a-link.txt"));
    // And still live afterwards.
    let w = state.watcher_status(sid).unwrap();
    assert!(wait_until(Duration::from_secs(10), || w.view().state
        == WatcherState::Live)
    .is_some());
    std::fs::write(e.root.join("after.txt"), b"x").unwrap();
    wait_until(Duration::from_secs(5), || exists(&state, sid, "after.txt"))
        .expect("live after reconcile");
    state.request_shutdown();
}

#[test]
fn unreachable_root_marks_offline_and_preserves() {
    let e = env();
    let state = open_state(&e.data);
    let sid = add_source(&state, &e.root);
    let p = start_scan(&state, sid).unwrap();
    wait_for_scan(&p, Duration::from_secs(30)).unwrap().unwrap();
    let files_before = state.catalog.source_counts(sid).unwrap().files;
    assert_eq!(files_before, 2);
    // Simulate disconnection by renaming the root away.
    state.request_shutdown();
    std::thread::sleep(Duration::from_millis(700));
    let gone = e.root.with_file_name("src-gone");
    std::fs::rename(&e.root, &gone).unwrap();
    let p = start_scan(&state, sid).unwrap();
    let r = wait_for_scan(&p, Duration::from_secs(30)).unwrap();
    assert!(r.is_err());
    let s = state.catalog.get_source(sid).unwrap().unwrap();
    assert_eq!(s.state, SourceState::Offline);
    assert_eq!(s.published_generation, Some(1));
    assert_eq!(
        state.catalog.source_counts(sid).unwrap().files,
        files_before,
        "last-known results preserved"
    );
    let c = state.catalog.source_completeness(sid).unwrap();
    assert!(!c.metadata_complete);
    assert_eq!(c.state, SourceState::Offline);
    std::fs::rename(&gone, &e.root).unwrap();
}
