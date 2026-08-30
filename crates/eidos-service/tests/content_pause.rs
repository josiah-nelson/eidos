//! Operator pause/resume for content extraction: claiming stops without
//! abandoning claimed work, the pause is durable across a restart, the
//! reported state matches what the workers are actually doing, and a paused
//! backlog does not hold reconciliation off forever.

use eidos_catalog::jobs::NewJob;
use eidos_catalog::NewSource;
use eidos_domain::{JobStage, Priority, SourceId, SourceKind};
use eidos_service::content_control::{
    content_status, ContentFlow, ContentPause, ContentSearchState, PAUSE_MARKER,
};
use eidos_service::content_workers::{claiming_allowed, reserve_and_claim};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

struct Env {
    _dir: tempfile::TempDir,
    data: PathBuf,
    state: Arc<AppState>,
}

fn open(data: &Path) -> Arc<AppState> {
    let cfg = ServiceConfig {
        data_dir: data.to_path_buf(),
        scan_threads: 1,
        auto_reconcile: false,
        content_workers: 1,
        fleet: false,
        ..Default::default()
    };
    Arc::new(AppState::open(&cfg).unwrap())
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    Env {
        state: open(&data),
        data,
        _dir: dir,
    }
}

/// A source with no scanned tree; only its identity matters here.
fn add_source(state: &AppState, name: &str) -> SourceId {
    state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: name.into(),
            kind: SourceKind::WindowsGeneric,
            root_path: format!("\\\\fileserver\\share\\{name}"),
            aliases: vec![],
        })
        .unwrap()
}

/// Queue `n` content jobs. They carry no object, so claiming is all these
/// tests exercise.
fn queue_jobs(state: &AppState, source: SourceId, n: u32) {
    let jobs: Vec<NewJob> = (0..n)
        .map(|i| NewJob {
            source_id: source,
            object_id: None,
            object_generation: 1,
            stage: JobStage::ContentText,
            priority: Priority::NormalText,
            idempotency_key: format!("content_text:{}:{i}", source.0),
            payload: None,
            estimated_cost: 0,
        })
        .collect();
    assert_eq!(state.catalog.enqueue_many(&jobs).unwrap(), n as usize);
}

#[test]
fn pausing_stops_claiming_and_resuming_starts_it_again() {
    let e = env();
    let sid = add_source(&e.state, "corpus");
    queue_jobs(&e.state, sid, 8);
    e.state.content_budgets().set(sid, 2);

    assert!(claiming_allowed(&e.state));
    let first = reserve_and_claim(&e.state, "content-0", 2)
        .unwrap()
        .expect("work is claimable before the pause");
    assert_eq!(first.1.len(), 2);

    e.state.content_pause.set_paused(true).unwrap();
    assert!(
        !claiming_allowed(&e.state),
        "a paused pipeline claims nothing more"
    );

    // The batch claimed a moment ago is untouched: still held, still
    // reserved, and free to finish. This is the whole point of gating the
    // claim rather than interrupting the worker.
    assert_eq!(first.0.source(), sid);
    assert_eq!(
        e.state.content_budgets().reserved(sid),
        1,
        "a pause does not release a reservation out from under a worker"
    );
    let status = content_status(&e.state);
    assert_eq!(status.flow, ContentFlow::Draining);
    assert!(status.paused);
    assert_eq!(status.in_flight, 1);

    // Finishing normally is what takes the pipeline from draining to stopped.
    drop(first);
    let status = content_status(&e.state);
    assert_eq!(status.flow, ContentFlow::Stopped);
    assert_eq!(status.in_flight, 0);

    e.state.content_pause.set_paused(false).unwrap();
    assert!(claiming_allowed(&e.state));
    assert_eq!(content_status(&e.state).flow, ContentFlow::Waiting);
    let resumed = reserve_and_claim(&e.state, "content-0", 2)
        .unwrap()
        .expect("the backlog is still there after a resume");
    assert_eq!(resumed.1.len(), 2, "queued work survived the pause");
}

#[test]
fn a_pause_survives_a_restart_and_the_queue_survives_with_it() {
    let e = env();
    let sid = add_source(&e.state, "corpus");
    queue_jobs(&e.state, sid, 4);
    e.state.content_budgets().set(sid, 1);
    e.state.content_pause.set_paused(true).unwrap();
    assert!(e.data.join(PAUSE_MARKER).exists());

    // Reopen the way a service restart does.
    drop(e.state);
    let restarted = open(&e.data);
    assert!(
        restarted.content_pause.is_paused(),
        "an operator who paused a busy volume does not get the load back \
         because the service was restarted"
    );
    assert!(!claiming_allowed(&restarted));
    assert_eq!(
        restarted
            .catalog
            .active_job_counts(sid, JobStage::ContentText)
            .unwrap()
            .0,
        4,
        "the durable backlog is intact"
    );

    restarted.content_pause.set_paused(false).unwrap();
    assert!(!e.data.join(PAUSE_MARKER).exists());
    drop(restarted);
    assert!(
        !open(&e.data).content_pause.is_paused(),
        "and a resume is durable too"
    );
}

// A paused backlog must not defer automatic reconciliation either. That
// lives beside its `--no-content` sibling in `watcher.rs`, where the fixture
// owns a scannable published source:
// `an_operator_paused_content_queue_does_not_block_automatic_reconciliation`.

#[test]
fn the_reported_state_names_the_condition_an_operator_can_act_on() {
    let e = env();

    let running = content_status(&e.state);
    assert_eq!(running.flow, ContentFlow::Waiting);
    assert_eq!(running.search, ContentSearchState::Ready);
    assert!(!running.paused);
    assert_eq!(running.paused_since_unix_s, None);

    e.state.content_pause.set_paused(true).unwrap();
    let paused = content_status(&e.state);
    assert_eq!(paused.flow, ContentFlow::Stopped);
    assert!(paused.paused_since_unix_s.is_some());
    assert!(
        paused.flow_reason.contains("paused"),
        "the reason names the pause: {}",
        paused.flow_reason
    );
    assert!(paused.detail.contains(&paused.flow_reason));

    // A rebuild outranks the pause because resume cannot restore claiming
    // while the rebuild owns the content-index writer.
    e.state.content_index.begin_rebuild(0).unwrap();
    let rebuilding = content_status(&e.state);
    assert_eq!(rebuilding.flow, ContentFlow::Waiting);
    assert_eq!(rebuilding.search, ContentSearchState::Rebuilding);
    assert!(rebuilding.flow_reason.contains("being rebuilt"));
    assert!(rebuilding.paused, "the durable pause remains recorded");

    // `--no-content` outranks a pause: resuming would change nothing, so
    // reporting "paused" would send the operator after the wrong switch.
    e.state.content_enabled.store(false, Ordering::Relaxed);
    let disabled = content_status(&e.state);
    assert_eq!(disabled.flow, ContentFlow::Disabled);
    assert_eq!(disabled.search, ContentSearchState::Disabled);
    assert!(
        disabled.paused,
        "the pause is still recorded, it is just not the operative reason"
    );
}

#[test]
fn a_pause_taken_before_the_service_opens_is_honoured_from_the_first_claim() {
    // The marker is what a restart reads, so a marker dropped in by hand --
    // or left by a crash mid-pause -- must gate claiming from the start,
    // before any worker thread has run.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join(PAUSE_MARKER), br#"{"paused_at_unix_s":1}"#).unwrap();

    let state = open(&data);
    assert!(state.content_pause.is_paused());
    assert_eq!(content_status(&state).flow, ContentFlow::Stopped);
    assert!(!claiming_allowed(&state));

    // And the loaded timestamp is the recorded one, not the restart's.
    assert_eq!(ContentPause::load(&data).paused_since_unix_s(), Some(1));
}
