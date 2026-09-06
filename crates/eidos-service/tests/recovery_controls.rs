//! Synthetic-only recovery faults. No installed service or source corpus.

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use eidos_catalog::NewSource;
use eidos_domain::{SourceId, SourceKind, SourceState};
use eidos_service::{
    content_workers::{
        claiming_allowed, commit_and_publish, refresh_budgets, reserve_and_claim, top_up_queue,
        CLAIM_BATCH,
    },
    resource_control::ResourceLimits,
    scanner::{run_full_scan, ScanProgress},
    state::AppState,
    ServiceConfig,
};
use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tower::ServiceExt;

struct NoSourceIo;
impl eidos_scanner::DirectoryLister for NoSourceIo {
    fn list(
        &self,
        _: &std::path::Path,
    ) -> Result<Vec<eidos_scanner::RawEntry>, eidos_scanner::ScanError> {
        panic!("source listing before admission");
    }
    fn volume_info(
        &self,
        _: &std::path::Path,
    ) -> Result<eidos_scanner::VolumeInfo, eidos_scanner::ScanError> {
        panic!("source capability/cursor probe before admission");
    }
    fn stat(
        &self,
        _: &std::path::Path,
    ) -> Result<eidos_scanner::RawEntry, eidos_scanner::ScanError> {
        panic!("source stat before admission");
    }
    fn name(&self) -> &'static str {
        "unadmitted fixture"
    }
}

fn fixture() -> (tempfile::TempDir, Arc<AppState>, SourceId) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    for name in ["alpha.txt", "beta.txt"] {
        std::fs::write(root.join(name), "fixture recovery publication text\n").unwrap();
    }
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.path().join("data"),
            fleet: false,
            auto_reconcile: false,
            update_check: false,
            ..Default::default()
        })
        .unwrap(),
    );
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "fixture".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    (dir, state, source)
}

#[test]
fn committed_content_survives_catalog_ack_failure_and_recovers_without_extraction() {
    let (dir, state, source) = fixture();
    run_full_scan(&state, source, &ScanProgress::new(source)).unwrap();
    assert_eq!(top_up_queue(&state).unwrap(), 2);
    for _ in 0..2 {
        let (_reservation, jobs) = reserve_and_claim(&state, "fixture", CLAIM_BATCH)
            .unwrap()
            .unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "pause must not leave a sixteen-file claimed backlog"
        );
        let job = &jobs[0];
        let object = job.object_id.unwrap();
        let result = eidos_search::pipeline::process_object(
            &state.catalog,
            &state.content_index,
            object,
            job.object_generation,
            &eidos_content::Limits::default(),
            Some(job.id),
        )
        .unwrap();
        assert!(matches!(
            result,
            eidos_search::pipeline::ProcessResult::Indexed(_)
        ));
        state.content_workers.pending_publish.lock().push(object);
    }
    state.catalog.with_writer(|conn| {
        conn.execute_batch("CREATE TRIGGER fail_ack BEFORE UPDATE OF state ON content_records
            WHEN NEW.state IN ('indexed', 'partial') BEGIN SELECT RAISE(ABORT, 'injected catalog fault'); END;")?;
        Ok(())
    }).unwrap();
    let ids = state.content_workers.pending_publish.lock().clone();
    for _ in 0..2 {
        assert!(commit_and_publish(&state)
            .unwrap_err()
            .to_string()
            .contains("injected catalog fault"));
        assert_eq!(*state.content_workers.pending_publish.lock(), ids);
        assert_eq!(
            state.content_index.uncommitted(),
            0,
            "index already committed"
        );
        assert_eq!(
            state.content_workers.commits.load(Ordering::Relaxed),
            1,
            "catalog-only retries must not repeatedly write clean index metadata"
        );
        assert!(
            !claiming_allowed(&state),
            "persistent store faults must backpressure new extraction"
        );
        assert!(
            !state
                .catalog
                .source_completeness(source)
                .unwrap()
                .content_complete
        );
    }
    state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch("DROP TRIGGER fail_ack")?;
            Ok(())
        })
        .unwrap();
    // Retry cannot rely on re-reading source bytes: the stored chunks and
    // committed index must suffice. These are temporary fixture files only.
    std::fs::remove_file(dir.path().join("root/alpha.txt")).unwrap();
    std::fs::remove_file(dir.path().join("root/beta.txt")).unwrap();
    assert_eq!(commit_and_publish(&state).unwrap(), 2);
    assert!(state.content_workers.pending_publish.lock().is_empty());
    assert!(claiming_allowed(&state));
    assert_eq!(
        state.catalog.get_source(source).unwrap().unwrap().state,
        SourceState::Complete
    );
    assert_eq!(
        commit_and_publish(&state).unwrap(),
        0,
        "acknowledgement is idempotent"
    );
}

#[test]
fn data_pressure_blocks_claims_and_queued_scans_cancel_without_opening_a_generation() {
    let (_dir, mut state, source) = fixture();
    let lister = state.lister.clone();
    Arc::get_mut(&mut state).unwrap().lister = Arc::new(NoSourceIo);
    refresh_budgets(&state).unwrap();
    state
        .resources
        .set(ResourceLimits {
            scan_threads: 1,
            concurrent_scans: 1,
            minimum_free_mib: u32::MAX,
        })
        .unwrap();
    assert!(!claiming_allowed(&state));
    assert!(eidos_service::content_control::content_status(&state)
        .flow_reason
        .contains("reserve"));
    let progress = Arc::new(ScanProgress::new(source));
    let worker = {
        let (state, progress) = (state.clone(), progress.clone());
        std::thread::spawn(move || {
            eidos_service::watcher::native_scan_sequence(&state, source, &progress)
                .err()
                .unwrap()
                .to_string()
        })
    };
    let started = Instant::now();
    while !progress.view().phase.contains("reserve") && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let phase = progress.view().phase;
    let cancelled = Instant::now();
    progress.cancel.store(true, Ordering::Relaxed);
    assert_eq!(worker.join().unwrap(), "scan cancelled");
    assert!(phase.contains("reserve"), "{phase}");
    assert!(cancelled.elapsed() < Duration::from_secs(1));
    assert!(state
        .catalog
        .open_scan_generation(source)
        .unwrap()
        .is_none());
    Arc::get_mut(&mut state).unwrap().lister = lister;
    state
        .resources
        .set(ResourceLimits {
            scan_threads: 1,
            concurrent_scans: 1,
            minimum_free_mib: 0,
        })
        .unwrap();
    assert!(claiming_allowed(&state));
    assert!(
        run_full_scan(&state, source, &ScanProgress::new(source))
            .unwrap()
            .published
    );
    assert_eq!(state.resources.view().active_scans, 0);
}

#[tokio::test]
async fn resource_api_validates_persists_and_reports_effective_limits() {
    let (_dir, state, _source) = fixture();
    let app = eidos_service::api::router(state.clone(), None);
    let initial = state.resources.view().limits;
    for (body, expected) in [
        (
            serde_json::json!({"scan_threads": 0, "concurrent_scans": 1, "minimum_free_mib": 0}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"scan_threads": 2, "concurrent_scans": 3, "minimum_free_mib": 2048}),
            StatusCode::OK,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/resources")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::BAD_REQUEST {
            assert_eq!(state.resources.view().limits, initial);
        }
    }
    let response = app
        .oneshot(Request::get("/api/resources").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(body["limits"]["scan_threads"], 2);
    assert_eq!(body["limits"]["concurrent_scans"], 3);
    assert!(body["free_bytes"].is_string(), "u64 wire format");
    assert_eq!(
        eidos_service::resource_control::ResourceControl::load(&state.data_dir, 8)
            .unwrap()
            .view()
            .limits,
        state.resources.view().limits
    );
}
