//! The global extraction pool: runtime resize, the durable operator
//! override, and clamping. Per-source budgets are covered by
//! `content_concurrency.rs`; this file is about pool size alone.

use eidos_service::content_workers::{
    load_workers_override, resize_workers, spawn_content_workers, MAX_WORKERS, WORKERS_MARKER,
};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<AppState> {
    Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.join("data"),
            scan_threads: 1,
            auto_reconcile: false,
            content_workers: 2,
            ..Default::default()
        })
        .unwrap(),
    )
}

#[test]
fn resize_is_durable_first_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state = open(dir.path());
    assert_eq!(
        state.content_worker_count, 2,
        "config default without a marker"
    );
    spawn_content_workers(&state, state.content_worker_count);
    assert_eq!(resize_workers(&state, 6).unwrap(), 6);
    assert_eq!(state.content_workers.view(0).workers, 6);
    assert!(dir.path().join("data").join(WORKERS_MARKER).exists());
    assert_eq!(load_workers_override(&dir.path().join("data")), Some(6));

    // Shrink: surplus threads park, the pool reports the operator's size,
    // and the spawned high-water mark is untouched.
    assert_eq!(resize_workers(&state, 3).unwrap(), 3);
    assert_eq!(state.content_workers.view(0).workers, 3);
    assert_eq!(
        state
            .content_workers
            .spawned
            .load(std::sync::atomic::Ordering::Relaxed),
        6
    );

    state.request_shutdown();
    // The workers and coordinator each hold an `Arc<AppState>`; the index
    // writer lock is only released when the last of them exits. Reopening
    // before that is the same double-open the serve process forbids.
    let mut waited = 0;
    while std::sync::Arc::strong_count(&state) > 1 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        waited += 1;
        assert!(waited < 200, "worker threads did not exit after shutdown");
    }
    drop(state);
    let state = open(dir.path());
    assert_eq!(
        state.content_worker_count, 3,
        "the marker overrides the config default at startup"
    );
    state.request_shutdown();
}

#[test]
fn the_pool_size_is_clamped_at_both_ends() {
    let dir = tempfile::tempdir().unwrap();
    let state = open(dir.path());
    spawn_content_workers(&state, 1);
    assert_eq!(resize_workers(&state, 0).unwrap(), 1);
    assert_eq!(resize_workers(&state, 10_000).unwrap(), MAX_WORKERS);
    assert_eq!(
        load_workers_override(&dir.path().join("data")),
        Some(MAX_WORKERS)
    );
    state.request_shutdown();
}

#[test]
fn a_corrupt_marker_falls_back_to_the_configured_size() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::write(dir.path().join("data").join(WORKERS_MARKER), b"not json").unwrap();
    assert_eq!(load_workers_override(&dir.path().join("data")), None);
    let state = open(dir.path());
    assert_eq!(state.content_worker_count, 2);
    state.request_shutdown();
}
