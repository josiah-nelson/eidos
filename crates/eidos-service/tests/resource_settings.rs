//! Coordinated resource API behavior on disposable settings directories.

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use eidos_catalog::NewSource;
use eidos_domain::SourceKind;
use eidos_service::{state::AppState, ServiceConfig};
use std::sync::Arc;
use tower::ServiceExt;

fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
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
    (dir, state)
}

async fn body(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap()
}

fn apply_body(scan_threads: u32, readers_per_device: u32) -> serde_json::Value {
    serde_json::json!({
        "settings": {
            "content_workers": 4,
            "scan_threads": scan_threads,
            "concurrent_scans": 1,
            "minimum_free_mib": 1024,
            "readers_per_device": readers_per_device
        }
    })
}

#[tokio::test]
async fn complete_tuple_applies_and_preserves_source_specific_caps() {
    let (_dir, state) = fixture();
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "synthetic source".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: "fixture-root".into(),
            aliases: Vec::new(),
        })
        .unwrap();
    state.catalog.set_content_policy(source, true, 7).unwrap();
    let app = eidos_service::api::router(state.clone(), None);
    let response = app
        .oneshot(
            Request::post("/api/resource-settings")
                .header("content-type", "application/json")
                .body(Body::from(apply_body(3, 1).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body(response).await;
    assert_eq!(body["outcome"], "applied");
    assert_eq!(body["current"]["scan_threads"], 3);
    assert_eq!(body["current"]["readers_per_device"], 1);
    assert!(body["pending"].is_null());
    assert_eq!(
        state
            .catalog
            .get_source(source)
            .unwrap()
            .unwrap()
            .content_concurrency,
        7,
        "the coordinated global tuple must leave source caps alone"
    );
}

#[tokio::test]
async fn partial_outcome_blocks_manual_edits_until_explicit_repair() {
    let (_dir, state) = fixture();
    std::fs::create_dir(state.data_dir.join("device-limits.json.tmp")).unwrap();
    let app = eidos_service::api::router(state.clone(), None);
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/resource-settings")
                .header("content-type", "application/json")
                .body(Body::from(apply_body(3, 1).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = body(response).await;
    assert_eq!(result["outcome"], "partial");
    assert_eq!(result["current"]["scan_threads"], 3);
    assert_eq!(result["current"]["readers_per_device"], 2);
    assert_eq!(result["pending"]["next_component"], "device_readers");

    let manual = app
        .clone()
        .oneshot(
            Request::post("/api/resources")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "scan_threads": 8,
                        "concurrent_scans": 1,
                        "minimum_free_mib": 1024
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manual.status(), StatusCode::CONFLICT);

    std::fs::remove_dir(state.data_dir.join("device-limits.json.tmp")).unwrap();
    let repaired = app
        .oneshot(
            Request::post("/api/resource-settings/repair")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repaired.status(), StatusCode::OK);
    let repaired = body(repaired).await;
    assert_eq!(repaired["outcome"], "applied");
    assert!(repaired["pending"].is_null());
}
