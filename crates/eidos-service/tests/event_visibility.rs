//! The index follower is event-driven: a completed catalog writer
//! transaction wakes it, so changes become searchable without waiting out a
//! poll interval. The follower here runs with a fallback interval far beyond
//! the test deadline, so visibility inside the deadline can only come from
//! the write signal.

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{SearchRequest, SourceKind};
use eidos_search::exec::{search, ExecOptions};
use eidos_service::follower::spawn_follower_with_fallback;
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn hits(state: &AppState, q: &str) -> usize {
    let parsed = eidos_query::parse(q).unwrap();
    search(
        &state.index,
        &state.catalog,
        &SearchRequest::new(parsed.query),
        &ExecOptions::default(),
    )
    .unwrap()
    .hits
    .len()
}

#[test]
fn catalog_write_wakes_follower_before_its_fallback_interval() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("first.txt"), b"one").unwrap();
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.path().join("data"),
            auto_reconcile: false,
            content_workers: 1,
            scan_threads: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let host = state.catalog.ensure_host("h", "windows").unwrap();
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: host,
            name: "fx".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    run_scan(
        &state.catalog,
        source,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    state.index.sync_sources(&state.catalog).unwrap();
    assert_eq!(hits(&state, "name:=first.txt"), 1);

    // A fallback of two minutes: if the follower still sleep-polled, the
    // change below would not be visible inside the deadline.
    spawn_follower_with_fallback(&state, Duration::from_secs(120));

    std::fs::write(root.join("second.txt"), b"two").unwrap();
    run_scan(
        &state.catalog,
        source,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    while hits(&state, "name:=second.txt") == 0 {
        assert!(
            Instant::now() < deadline,
            "the write signal did not wake the follower; outbox pending = {:?}",
            state.catalog.outbox_pending()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Wake the follower one more time so it observes shutdown and exits
    // instead of blocking out its long fallback while the tempdir drops.
    state.shutdown.store(true, Ordering::Relaxed);
    state.catalog.with_writer(|_| Ok(())).unwrap();
    std::thread::sleep(Duration::from_millis(50));
}
