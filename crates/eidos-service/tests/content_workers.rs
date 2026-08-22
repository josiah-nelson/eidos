//! Milestone 4 end-to-end: content workers drain a scanned source, publish
//! through batched commits, survive a restart without re-extraction, and
//! respect the per-source content policy.

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{ContentState, SearchRequest, SourceId, SourceKind};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_service::content_workers::spawn_content_workers;
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    for i in 0..40 {
        std::fs::write(
            root.join(format!("docs/note{i}.md")),
            format!("note {i}\nshared marker alpha\nunique token omega{i}\n"),
        )
        .unwrap();
    }
    std::fs::write(root.join("a.txt"), b"plain text with Zephyr inside\n").unwrap();
    std::fs::write(root.join("blob"), b"\x00\x01\x02binary\x00\x00").unwrap();
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
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn search(state: &AppState, q: &str) -> eidos_domain::SearchResponse {
    let parsed = eidos_query::parse(q).unwrap();
    let mut r = SearchRequest::new(parsed.query);
    r.explain = true;
    search_with_content(
        &state.index,
        Some(&state.content_index),
        &state.catalog,
        &r,
        &ExecOptions::default(),
    )
    .unwrap()
}

fn refresh_index(state: &AppState) {
    eidos_service::follower::follow_once(state).unwrap();
    state.index.reload().unwrap();
}

#[test]
fn workers_drain_publish_and_survive_restart() {
    let e = env();
    let state = open_state(&e.data);
    let sid: SourceId = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "fixture".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: e.root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    run_scan(
        &state.catalog,
        sid,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    refresh_index(&state);
    let before = state.catalog.source_completeness(sid).unwrap();
    assert!(!before.content_complete);
    assert_eq!(
        before.content_pending, 42,
        "40 notes + a.txt + extensionless blob (sniffed later)"
    );

    spawn_content_workers(&state, 2);
    // Coordinator tops up the queue every 5 s; nudge it now.
    eidos_service::content_workers::top_up_queue(&state).unwrap();
    assert!(
        wait_until(Duration::from_secs(30), || {
            state
                .catalog
                .source_completeness(sid)
                .map(|c| c.content_complete)
                .unwrap_or(false)
        }),
        "content never completed: {:?}",
        state
            .content_workers
            .view(state.content_index.uncommitted())
    );
    let view = state.content_workers.view(0);
    assert_eq!(view.files_indexed, 41, "{view:?}");
    assert_eq!(view.files_unsupported, 1, "{view:?}");
    assert_eq!(view.files_failed, 0);
    assert!(view.commits >= 1);
    assert_eq!(view.published, 41);

    refresh_index(&state);
    let r = search(&state, "content:omega7");
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].name, "note7.md");
    assert!(r.all_sources_complete(true));
    let r = search(&state, "content:alpha");
    assert_eq!(r.total.value, 40);
    assert_eq!(search(&state, "state:unsupported").hits[0].name, "blob");

    // Restart: nothing is re-extracted, content still searchable.
    state.request_shutdown();
    std::thread::sleep(Duration::from_millis(800));
    drop(state);
    let state = open_state(&e.data);
    assert!(!state.content_index.is_fresh(), "content index persisted");
    spawn_content_workers(&state, 2);
    eidos_service::content_workers::top_up_queue(&state).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let view = state.content_workers.view(0);
    assert_eq!(
        view.files_indexed, 0,
        "restart must not re-extract: {view:?}"
    );
    refresh_index(&state);
    assert_eq!(search(&state, "content:Zephyr").hits[0].name, "a.txt");
    assert!(
        state
            .catalog
            .source_completeness(sid)
            .unwrap()
            .content_complete
    );

    // Policy: disabling drops queued work; a changed file waits until re-enabled.
    state.catalog.set_content_policy(sid, false, 1).unwrap();
    std::fs::write(
        e.root.join("a.txt"),
        b"changed text with Zephyr and newtoken\n",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    run_scan(
        &state.catalog,
        sid,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    refresh_index(&state);
    let r = search(&state, "name:=a.txt");
    assert_eq!(r.hits[0].content.state, ContentState::Pending);
    eidos_service::content_workers::top_up_queue(&state).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        state.content_workers.view(0).files_indexed,
        0,
        "disabled source must not be extracted"
    );
    state.catalog.set_content_policy(sid, true, 2).unwrap();
    eidos_service::content_workers::top_up_queue(&state).unwrap();
    assert!(wait_until(Duration::from_secs(20), || {
        state.content_workers.files_indexed.load(Ordering::Relaxed) >= 1
            && state
                .catalog
                .source_completeness(sid)
                .map(|c| c.content_complete)
                .unwrap_or(false)
    }));
    refresh_index(&state);
    assert_eq!(search(&state, "content:newtoken").hits[0].name, "a.txt");
    state.request_shutdown();
}
