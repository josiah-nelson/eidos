//! Current-process diagnostics only; no source enumeration or installed data.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use eidos_service::{state::AppState, ServiceConfig};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn memory_api_is_read_only_uses_exact_counters_and_exposes_runtime_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.path().to_owned(),
            content: false,
            fleet: false,
            auto_reconcile: false,
            update_check: false,
            ..Default::default()
        })
        .unwrap(),
    );
    let app = eidos_service::api::router(state.clone(), None);
    let writes = state.catalog.writer_stats().acquisitions;
    // One request, cold cache: a client that cannot poll (`eidos resources
    // --memory`) must still receive a sample rather than an empty placeholder.
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/memory")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["catalog"]["baseline_connections"], 13);
        assert_eq!(
            body["catalog"]["page_cache_baseline_target_bytes"],
            (13_u64 * 64 * 1024 * 1024).to_string()
        );
        assert_eq!(
            body["catalog_writer_budget_bytes"],
            eidos_search::CATALOG_WRITER_MEMORY_BYTES.to_string()
        );
        assert_eq!(
            body["content_writer_budget_bytes"],
            eidos_search::content::CONTENT_WRITER_MEMORY_BYTES.to_string()
        );
        assert_eq!(
            body["content_input_budget_bytes"],
            eidos_search::content::CONTENT_INPUT_MEMORY_BYTES.to_string()
        );
        assert!(
            !body["process"].is_null(),
            "the first request must carry a process sample: {body}"
        );
        assert_eq!(body["process"]["pid"], std::process::id());
        assert!(
            body["process"]["resident_bytes"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        assert_eq!(body["stale"], false);
        // The second request is served from the cache, without a new probe.
        assert!(!body["sample_age_s"].is_null());
    }
    assert_eq!(
        state.catalog.writer_stats().acquisitions,
        writes,
        "diagnostics must not wake the writer/follower"
    );
}
