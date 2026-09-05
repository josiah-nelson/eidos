//! One deterministic restart contract spanning every durable M5 layer.
//!
//! Existing focused tests prove each recovery mechanism separately. This
//! fixture deliberately puts all of them in one data directory before the
//! restart so startup cannot fix one subsystem by rebuilding or discarding
//! healthy state owned by another.

#![cfg(windows)]

use eidos_archive::fixture::{build, Entry};
use eidos_catalog::scan::{run_scan, RunScanOptions, ScanKind};
use eidos_catalog::NewSource;
use eidos_content::Limits;
use eidos_domain::{
    ContentState, FileAttributes, JobId, JobStage, JobState, ObjectId, ObjectKind, SearchRequest,
    SourceId, SourceKind, SourceState,
};
use eidos_scanner::{DirEvent, DirToken, RawEntry};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_search::pipeline::{drain_content_jobs, process_object, ProcessResult};
use eidos_service::state::{AppState, StartupRecovery};
use eidos_service::ServiceConfig;
use http_body_util::BodyExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/stable.txt"),
        b"StableRetentionToken survives a clean index reopen.\n",
    )
    .unwrap();
    let archive = build(
        &[
            Entry::dir("nested/"),
            Entry::file("nested/member.retained", b"virtual payload"),
        ],
        b"restart retention fixture",
        false,
    );
    std::fs::write(root.join("bundle.zip"), archive).unwrap();
    Fixture {
        data: dir.path().join("data"),
        _dir: dir,
        root,
    }
}

fn open_state(data: &Path) -> Arc<AppState> {
    Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: data.to_path_buf(),
            auto_reconcile: false,
            content_workers: 1,
            scan_threads: 1,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn open_auto_state(data: &Path) -> Arc<AppState> {
    Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: data.to_path_buf(),
            auto_reconcile: true,
            content_workers: 1,
            scan_threads: 1,
            fleet: false,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn scan(state: &AppState, source: SourceId) -> i64 {
    run_scan(
        &state.catalog,
        source,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap()
    .generation
}

fn search(state: &AppState, text: &str) -> eidos_domain::SearchResponse {
    let parsed = eidos_query::parse(text).unwrap();
    search_with_content(
        &state.index,
        Some(&state.content_index),
        &state.catalog,
        &SearchRequest::new(parsed.query),
        &ExecOptions::default(),
    )
    .unwrap()
}

fn object(state: &AppState, source: SourceId, relative: &str) -> ObjectId {
    state
        .catalog
        .resolve_relative(source, relative)
        .unwrap()
        .unwrap_or_else(|| panic!("{relative} not found"))
}

fn only_queued_job(state: &AppState) -> JobId {
    state
        .catalog
        .with_reader(|conn| {
            Ok(JobId(conn.query_row(
                "SELECT job_id FROM jobs WHERE state = 'queued'",
                [],
                |row| row.get(0),
            )?))
        })
        .unwrap()
}

#[tokio::test]
async fn restart_retains_published_truth_and_repairs_only_interrupted_work() {
    let fixture = fixture();
    let state = open_state(&fixture.data);
    assert_eq!(state.startup_recovery, StartupRecovery::default());
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "restart-fixture".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: fixture.root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();

    // Establish healthy published state in every derived layer: catalog
    // generation, catalog projection, stored text/content index, and archive
    // virtual objects. No background thread participates in the setup.
    assert_eq!(scan(&state, source), 1);
    assert_eq!(
        state.catalog.enqueue_pending_content(source, 100).unwrap(),
        2
    );
    assert_eq!(
        drain_content_jobs(
            &state.catalog,
            &state.content_index,
            &Limits::default(),
            "fixture-initial"
        )
        .unwrap(),
        1,
        "the text file publishes content; the ZIP publishes a manifest"
    );
    eidos_service::follower::follow_once(&state).unwrap();

    let stable = object(&state, source, "docs/stable.txt");
    let archive = object(&state, source, "bundle.zip");
    let virtual_member = object(&state, source, "bundle.zip/nested/member.retained");
    assert_eq!(
        state
            .catalog
            .get_object(stable)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Indexed
    );
    assert_eq!(
        state
            .catalog
            .get_object(virtual_member)
            .unwrap()
            .unwrap()
            .kind,
        ObjectKind::VirtualFile
    );
    assert_eq!(
        state
            .catalog
            .archive_record(archive)
            .unwrap()
            .unwrap()
            .member_count,
        2
    );
    assert_eq!(
        search(&state, "content:=StableRetentionToken").hits.len(),
        1
    );
    assert_eq!(search(&state, "name:=member.retained").hits.len(), 1);

    // Publish a second metadata generation with three new files. One job will
    // stay queued, one will look owned by a crashed worker, and one will have
    // stored chunks plus an `indexing` record but no content-index commit.
    for name in ["queued.txt", "running.txt", "indexing.txt"] {
        std::fs::write(
            fixture.root.join("docs").join(name),
            format!("UnpublishedRetentionToken from {name}\n"),
        )
        .unwrap();
    }
    assert_eq!(scan(&state, source), 2);
    eidos_service::follower::follow_once(&state).unwrap();
    assert_eq!(
        state.catalog.enqueue_pending_content(source, 100).unwrap(),
        3
    );

    let running = state
        .catalog
        .claim_job(&[JobStage::ContentText], "worker-lost-at-restart")
        .unwrap()
        .unwrap();
    let indexing = state
        .catalog
        .claim_job(&[JobStage::ContentText], "worker-before-index-commit")
        .unwrap()
        .unwrap();
    let indexing_object = indexing.object_id.unwrap();
    match process_object(
        &state.catalog,
        &state.content_index,
        indexing_object,
        indexing.object_generation,
        &Limits::default(),
        Some(indexing.id),
    )
    .unwrap()
    {
        ProcessResult::Indexed(stats) => assert_eq!(stats.object_id, indexing_object),
        other => panic!("expected an uncommitted indexed result, got {other:?}"),
    }
    let queued = only_queued_job(&state);
    assert_eq!(
        state
            .catalog
            .active_job_counts(source, JobStage::ContentText)
            .unwrap(),
        (1, 1)
    );
    let stored_state: String = state
        .catalog
        .with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT state FROM content_records WHERE object_id = ?1",
                [indexing_object.0],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(stored_state, "indexing");
    assert!(state.content_index.uncommitted() > 0);
    assert!(search(&state, "content:=UnpublishedRetentionToken")
        .hits
        .is_empty());

    // Snapshot the healthy publication boundary. Generation 3 is opened but
    // deliberately leaked: process termination skips Drop, unlike ordinary
    // cancellation, so the next AppState::open must perform crash recovery.
    let published_counts = state.catalog.source_counts(source).unwrap();
    let catalog_documents = state.index.num_docs();
    let content_documents = state.content_index.num_docs();
    let projection = state
        .catalog
        .projection_source(eidos_search::PROJECTION_NAME, source)
        .unwrap()
        .unwrap();
    assert_eq!(projection.generation, 2);
    assert_eq!(projection.documents, catalog_documents);
    let interrupted = state
        .catalog
        .begin_scan(source, ScanKind::Reconcile)
        .unwrap();
    let interrupted_generation = interrupted.generation();
    assert_eq!(interrupted_generation, 3);
    std::mem::forget(interrupted);
    let before_restart = state.catalog.get_source(source).unwrap().unwrap();
    assert_eq!(before_restart.published_generation, Some(2));
    assert_eq!(before_restart.state, SourceState::Reconciling);
    drop(state);

    // Startup repair is synchronous: callers can observe a truthful source
    // and queue before starting any follower, watcher, or content worker.
    let state = open_state(&fixture.data);
    assert_eq!(
        state.startup_recovery,
        StartupRecovery {
            aborted_scan_generations: 1,
            requeued_running_jobs: 1,
            requeued_unfinished_content: 1,
        }
    );
    assert!(
        !state.content_rebuild,
        "healthy content index must be retained"
    );
    assert_eq!(state.index.num_docs(), catalog_documents);
    assert_eq!(state.content_index.num_docs(), content_documents);
    assert_eq!(
        state.catalog.source_counts(source).unwrap(),
        published_counts
    );

    let recovered_source = state.catalog.get_source(source).unwrap().unwrap();
    assert_eq!(recovered_source.published_generation, Some(2));
    assert_eq!(recovered_source.state, SourceState::Degraded);
    assert!(recovered_source
        .state_reason
        .as_deref()
        .unwrap_or_default()
        .contains("recovered at startup"));
    assert_eq!(state.catalog.open_scan_generation(source).unwrap(), None);
    let generations = state.catalog.list_generations(source, 3).unwrap();
    assert_eq!(generations[0].generation, interrupted_generation);
    assert_eq!(generations[0].state, "aborted");
    assert_eq!(generations[1].generation, 2);
    assert_eq!(generations[1].state, "published");

    // The originally queued job is untouched. The running job is due and
    // queued again with its attempt history, while the completed job behind
    // the unfinished `indexing` record was replaced by a fresh queued job.
    let queued_after = state.catalog.get_job(queued).unwrap().unwrap();
    assert_eq!(queued_after.state, JobState::Queued);
    assert_eq!(queued_after.attempts, 0);
    let running_after = state.catalog.get_job(running.id).unwrap().unwrap();
    assert_eq!(running_after.state, JobState::Queued);
    assert_eq!(running_after.attempts, 1);
    assert!(running_after.worker.is_none());
    assert!(state.catalog.get_job(indexing.id).unwrap().is_none());
    assert_eq!(
        state
            .catalog
            .active_job_counts(source, JobStage::ContentText)
            .unwrap(),
        (3, 0)
    );

    // Healthy projections reopen without a source rebuild. A no-op follower
    // iteration leaves the exact projection record and document counts alone.
    eidos_service::follower::follow_once(&state).unwrap();
    assert_eq!(
        state.follower.rebuilds.load(Ordering::Relaxed),
        0,
        "restart catch-up must not rebuild a healthy catalog projection"
    );
    assert_eq!(state.follower.rows_applied.load(Ordering::Relaxed), 0);
    assert_eq!(
        state
            .catalog
            .projection_source(eidos_search::PROJECTION_NAME, source)
            .unwrap()
            .unwrap(),
        projection
    );
    assert_eq!(
        search(&state, "content:=StableRetentionToken").hits.len(),
        1
    );
    let virtual_hits = search(&state, "name:=member.retained");
    assert_eq!(virtual_hits.hits.len(), 1);
    assert_eq!(virtual_hits.hits[0].kind, ObjectKind::VirtualFile);
    assert!(state
        .catalog
        .resolve_relative(source, "bundle.zip/nested/member.retained")
        .unwrap()
        .is_some());
    assert_eq!(
        state
            .catalog
            .archive_record(archive)
            .unwrap()
            .unwrap()
            .member_count,
        2
    );
    assert!(search(&state, "content:=UnpublishedRetentionToken")
        .hits
        .is_empty());

    // The same synchronous repair report and recovered queue are observable
    // through the public Activity contract, before background work changes
    // either count. API u64 values use the schema-v2 decimal-string form.
    let response = eidos_service::api::router(state.clone(), None)
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/activity")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["startup_recovery"]["aborted_scan_generations"], "1");
    assert_eq!(body["startup_recovery"]["requeued_running_jobs"], "1");
    assert_eq!(body["startup_recovery"]["requeued_unfinished_content"], "1");
    assert_eq!(body["jobs"]["queued"], "3");
    assert_eq!(body["jobs"]["running"], "0");
}

#[test]
fn interrupted_initial_generation_is_retried_and_published_after_restart() {
    let fixture = fixture();
    let state = open_auto_state(&fixture.data);
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "initial-recovery".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: fixture.root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();

    // Commit one visible batch but leak the still-open generation exactly as
    // process termination would. This is the state that used to strand a
    // source forever: no publication, pending objects, and no runnable jobs.
    let mut interrupted = state.catalog.begin_scan(source, ScanKind::Full).unwrap();
    interrupted
        .ingest(DirEvent {
            token: DirToken(0),
            parent: None,
            path: fixture.root.clone(),
            depth: 0,
            result: Ok(vec![RawEntry {
                name: "crash-window.txt".into(),
                name_lossy: false,
                kind: ObjectKind::File,
                attributes: FileAttributes::default(),
                size: 12,
                allocated: Some(12),
                created: None,
                modified: None,
                changed: None,
                accessed: None,
                native_id: None,
                reparse_tag: 0,
            }]),
            child_tokens: vec![],
        })
        .unwrap();
    interrupted.commit().unwrap();
    assert_eq!(
        state.catalog.source_counts(source).unwrap().content_pending,
        1
    );
    std::mem::forget(interrupted);
    drop(state);

    let state = open_auto_state(&fixture.data);
    assert_eq!(state.startup_recovery.aborted_scan_generations, 1);
    let recovered = state.catalog.get_source(source).unwrap().unwrap();
    assert_eq!(recovered.published_generation, None);
    assert_eq!(recovered.state, SourceState::Degraded);
    assert!(recovered
        .state_reason
        .as_deref()
        .unwrap_or_default()
        .contains("recovered at startup"));

    state.start_background().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let current = state.catalog.get_source(source).unwrap().unwrap();
        if current.published_generation.is_some() {
            assert_ne!(current.state, SourceState::Degraded);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the recovered initial scan never published: {current:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(state.catalog.open_scan_generation(source).unwrap(), None);
    assert_eq!(state.catalog.source_counts(source).unwrap().files, 2);
    state.request_shutdown();
}
