//! Synthetic catalog jobs and temporary local state; no share/source I/O.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use eidos_catalog::{jobs::NewJob, NewSource};
use eidos_domain::{JobStage, Priority, SourceId, SourceKind};
use eidos_service::{
    content_workers::{refresh_budgets, reserve_and_claim},
    device_control::{DeviceControl, DeviceLimits},
    state::AppState,
    ServiceConfig,
};
use std::sync::Arc;
use tower::ServiceExt;

struct WidthLister {
    inner: Box<dyn eidos_scanner::DirectoryLister>,
    active: std::sync::atomic::AtomicUsize,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl eidos_scanner::DirectoryLister for WidthLister {
    fn list(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<eidos_scanner::RawEntry>, eidos_scanner::ScanError> {
        use std::sync::atomic::Ordering::SeqCst;
        let active = self.active.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(active, SeqCst);
        struct Release<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, SeqCst);
            }
        }
        let _release = Release(&self.active);
        std::thread::sleep(std::time::Duration::from_millis(5));
        self.inner.list(path)
    }
    fn stat(
        &self,
        path: &std::path::Path,
    ) -> Result<eidos_scanner::RawEntry, eidos_scanner::ScanError> {
        self.inner.stat(path)
    }
    fn volume_info(
        &self,
        path: &std::path::Path,
    ) -> Result<eidos_scanner::VolumeInfo, eidos_scanner::ScanError> {
        self.inner.volume_info(path)
    }
    fn name(&self) -> &'static str {
        "bounded width fixture"
    }
}

#[test]
fn an_eight_thread_scan_uses_only_the_width_left_by_content_work() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    for i in 0..12 {
        std::fs::create_dir(root.join(format!("branch-{i}"))).unwrap();
    }
    let mut state = AppState::open(&ServiceConfig {
        data_dir: dir.path().join("data"),
        scan_threads: 8,
        fleet: false,
        update_check: false,
        auto_reconcile: false,
        ..Default::default()
    })
    .unwrap();
    let peak = Arc::new(AtomicUsize::new(0));
    state.lister = Arc::new(WidthLister {
        inner: eidos_scanner::default_lister(),
        active: AtomicUsize::new(0),
        peak: peak.clone(),
    });
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "width fixture".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.to_string_lossy().into_owned(),
            aliases: vec![],
        })
        .unwrap();
    refresh_budgets(&state).unwrap();
    let content = state
        .devices
        .try_reserve(source.0, eidos_service::device_budget::WorkKind::Content, 1)
        .unwrap();
    let state = Arc::new(state);
    let progress = eidos_service::scanner::ScanProgress::new(source);
    eidos_service::scanner::run_full_scan(&state, source, &progress).unwrap();
    assert_eq!(
        peak.load(SeqCst),
        1,
        "scan exceeded the one remaining reader unit"
    );
    let budget = state.devices.view().budget;
    assert_eq!(budget.devices[0].content_readers, 1);
    assert_eq!(budget.devices[0].scan_threads, 0);
    assert_eq!(state.resources.view().active_scans, 0);
    drop(content);
    assert_eq!(state.devices.view().budget.devices[0].content_readers, 0);
}

fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.path().join("data"),
            fleet: false,
            update_check: false,
            auto_reconcile: false,
            ..Default::default()
        })
        .unwrap(),
    );
    (dir, state)
}

fn source(state: &AppState, name: &str) -> SourceId {
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: name.into(),
            kind: SourceKind::WindowsGeneric,
            root_path: format!(r"\\fileserver\share\{name}"),
            aliases: vec![],
        })
        .unwrap();
    state.catalog.set_content_policy(source, true, 1).unwrap();
    state
        .catalog
        .enqueue_many(
            &(0..4)
                .map(|i| NewJob {
                    source_id: source,
                    object_id: None,
                    object_generation: 1,
                    stage: JobStage::ContentText,
                    priority: Priority::NormalText,
                    idempotency_key: format!("device-fixture:{}:{i}", source.0),
                    payload: None,
                    estimated_cost: 0,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    source
}

#[test]
fn real_job_claims_charge_one_shared_budget_across_source_roots_and_release() {
    let (_dir, state) = fixture();
    let a = source(&state, "first");
    let b = source(&state, "second");
    refresh_budgets(&state).unwrap();
    state.devices.set_limit(2).unwrap();
    let first = reserve_and_claim(&state, "one", 1).unwrap().unwrap();
    let second = reserve_and_claim(&state, "two", 1).unwrap().unwrap();
    assert_ne!(first.0.source(), second.0.source());
    let snapshot = state.devices.view();
    assert!(snapshot.budget.unresolved_shared_fallback);
    assert_eq!(snapshot.budget.devices[0].sources, vec![a.0, b.0]);
    assert_eq!(snapshot.budget.devices[0].content_readers, 2);
    assert!(reserve_and_claim(&state, "three", 1).unwrap().is_none());
    state.devices.set_limit(1).unwrap();
    drop(first);
    assert!(reserve_and_claim(&state, "three", 1).unwrap().is_none());
    drop(second);
    assert_eq!(state.devices.view().budget.devices[0].content_readers, 0);
    let next = reserve_and_claim(&state, "three", 1).unwrap().unwrap();
    drop(next);
    assert_eq!(state.devices.view().budget.devices[0].peak_readers, 2);
}

#[test]
fn a_device_refusal_does_not_inflate_per_source_peak_reservations() {
    let (_dir, state) = fixture();
    let a = source(&state, "device-blocked-first");
    let b = source(&state, "device-blocked-second");
    refresh_budgets(&state).unwrap();
    state.devices.set_limit(1).unwrap();
    // One admitted claim exhausts the shared budget both sources are charged
    // against; every later attempt is refused by the device, not the source.
    let held = reserve_and_claim(&state, "one", 1).unwrap().unwrap();
    for _ in 0..8 {
        assert!(reserve_and_claim(&state, "two", 1).unwrap().is_none());
    }
    let budgets = state.content_budgets();
    assert_eq!(
        budgets.peak_reserved(a) + budgets.peak_reserved(b),
        1,
        "a shared-device refusal must not raise another source's peak reservation"
    );
    assert_eq!(state.devices.view().budget.devices[0].peak_readers, 1);
    drop(held);
    assert!(reserve_and_claim(&state, "three", 1).unwrap().is_some());
}

#[test]
fn a_catalog_claim_failure_releases_both_reservations() {
    let (_dir, state) = fixture();
    let source = source(&state, "failure");
    refresh_budgets(&state).unwrap();
    state.catalog.with_writer(|conn| { conn.execute_batch("CREATE TRIGGER reject_claim BEFORE UPDATE ON jobs BEGIN SELECT RAISE(ABORT, 'synthetic claim failure'); END")?; Ok(()) }).unwrap();
    assert!(reserve_and_claim(&state, "one", 1).is_err());
    assert_eq!(state.content_budgets().reserved(source), 0);
    assert_eq!(state.devices.view().budget.devices[0].content_readers, 0);
    // The units were released without a file being read, so the reported
    // high-water mark must not describe a claim that never committed.
    assert_eq!(state.content_budgets().peak_reserved(source), 0);
    state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch("DROP TRIGGER reject_claim")?;
            Ok(())
        })
        .unwrap();
    let claimed = reserve_and_claim(&state, "two", 1).unwrap();
    assert!(claimed.is_some());
    assert_eq!(state.content_budgets().peak_reserved(source), 1);
}

#[tokio::test]
async fn device_api_validates_and_persists_without_resetting_other_resource_limits() {
    let (_dir, state) = fixture();
    let source = source(&state, "api-fixture");
    refresh_budgets(&state).unwrap();
    let resources = state.resources.view().limits;
    let app = eidos_service::api::router(state.clone(), None);
    for (limit, expected) in [
        (0, StatusCode::BAD_REQUEST),
        (65, StatusCode::BAD_REQUEST),
        (3, StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/devices")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"readers_per_device":{limit}}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(state.resources.view().limits, resources);
    assert_eq!(
        DeviceControl::load(&state.data_dir)
            .unwrap()
            .view()
            .budget
            .readers_per_device,
        3
    );
    let before = state.catalog.writer_stats().acquisitions;
    let response = app
        .oneshot(Request::get("/api/devices").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(body["budget"]["readers_per_device"], 3);
    assert_eq!(
        body["source_roots"][source.0.to_string()],
        r"\\fileserver\share\api-fixture"
    );
    assert!(body["source_errors"].is_object());
    assert_eq!(
        body["budget"]["devices"][0]["sources"][0],
        source.0.to_string()
    );
    assert_eq!(state.catalog.writer_stats().acquisitions, before);
}

#[test]
fn failed_persistence_keeps_the_effective_limit_and_corrupt_settings_fail_closed() {
    let (dir, state) = fixture();
    std::fs::create_dir(state.data_dir.join("device-limits.json.tmp")).unwrap();
    assert!(state
        .devices
        .save_limits(
            &state.data_dir,
            DeviceLimits {
                readers_per_device: 4
            }
        )
        .is_err());
    assert_eq!(state.devices.view().budget.readers_per_device, 2);
    std::fs::write(dir.path().join("device-limits.json"), b"invalid").unwrap();
    assert!(DeviceControl::load(dir.path()).is_err());
    std::fs::write(dir.path().join("device-limits.json"), vec![b' '; 4097]).unwrap();
    assert!(DeviceControl::load(dir.path()).is_err());
}
