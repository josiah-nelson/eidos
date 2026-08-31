//! A real HTTP boundary for the two typed CLI clients. The service's API v2
//! writer emits i64/u64 values as decimal strings; these tests fail if the
//! CLI only works against ordinary serde_json numeric values.

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::SourceKind;
use eidos_fleet::{Fleet, FleetConfig, NodeIdentity};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::process::{Command, Output};
use std::sync::Arc;

const EXE: &str = env!("CARGO_BIN_EXE_eidos");

fn cli(args: &[&str]) -> Output {
    Command::new(EXE).args(args).output().unwrap()
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_search_and_fleet_clients_accept_api_v2_integer_strings() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("boundary.txt"), b"api v2 boundary").unwrap();

    NodeIdentity::load_or_create(&data, "api-v2-test").unwrap();
    FleetConfig::default().store(&data).unwrap();
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: data.clone(),
            scan_threads: 1,
            auto_reconcile: false,
            content: false,
            fleet: false,
            ..Default::default()
        })
        .unwrap(),
    );
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "boundary".into(),
            kind: SourceKind::WindowsLocal,
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
    eidos_service::follower::follow_once(&state).unwrap();

    let fleet = Fleet::start(state.catalog.clone(), &data).unwrap();
    *state.fleet.lock() = Some(fleet);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = eidos_service::api::router(state.clone(), None);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.await;
            })
            .await
    });
    let url = format!("http://{address}");

    let search = cli(&[
        "--log", "warn", "search", "--url", &url, "--json", "ext:txt",
    ]);
    assert_success(&search, "search");
    let search_json: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(search_json["hits"][0]["name"], "boundary.txt");
    assert!(search_json["total"]["value"].is_string());

    let fleet = cli(&["--log", "warn", "fleet", "--url", &url, "--json", "status"]);
    assert_success(&fleet, "fleet status");
    let fleet_json: serde_json::Value = serde_json::from_slice(&fleet.stdout).unwrap();
    assert_eq!(fleet_json["name"], "api-v2-test");
    // CLI JSON is reserialized from FleetStatus. Reaching this assertion is
    // the important boundary: the service response itself contained strings.
    assert!(fleet_json["join_requests"].is_array());

    state.request_shutdown();
    let _ = stop_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("HTTP server did not stop")
        .unwrap()
        .unwrap();
}
