//! Operator retry over HTTP: the endpoints report accepted/skipped/rejected
//! counts, stay idempotent, and a retried job is really re-run by the
//! content workers.

#![cfg(windows)]

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{FailureClass, JobStage, SearchRequest, SourceId, SourceKind};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_service::content_workers::{spawn_content_workers, top_up_queue};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const TEXT: &[u8] = b"report for Zephyr\nunique token qzendpoint\n";

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), TEXT).unwrap();
    Env {
        data: dir.path().join("data"),
        _dir: dir,
        root,
    }
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

/// Drive one API request through the real router.
fn send(rt: &tokio::runtime::Runtime, app: &axum::Router, uri: &str, body: &str) -> (u16, Vec<u8>) {
    rt.block_on(async {
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, bytes.to_vec())
    })
}

fn post(
    rt: &tokio::runtime::Runtime,
    app: &axum::Router,
    uri: &str,
    body: &str,
) -> serde_json::Value {
    let (status, bytes) = send(rt, app, uri, body);
    assert!(
        (200..300).contains(&status),
        "{uri} -> {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

fn status(rt: &tokio::runtime::Runtime, app: &axum::Router, uri: &str, body: &str) -> u16 {
    send(rt, app, uri, body).0
}

fn n(v: &serde_json::Value, key: &str) -> u64 {
    v[key]
        .as_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{key} in {v}"))
}

#[test]
fn retry_endpoints_requeue_a_failed_job_that_workers_then_process() {
    let e = env();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: e.data.clone(),
            scan_threads: 2,
            auto_reconcile: false,
            content_workers: 2,
            ..Default::default()
        })
        .unwrap(),
    );
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
    let app = eidos_service::api::router(state.clone(), None);

    // A worker claims the only content job and fails it deterministically:
    // automatic backoff will never bring it back.
    top_up_queue(&state).unwrap();
    let job = state
        .catalog
        .claim_job(&[JobStage::ContentText], "w")
        .unwrap()
        .expect("one content job");
    state
        .catalog
        .fail_job(job.id, FailureClass::Deterministic, "extract: no decoder")
        .unwrap();
    assert_eq!(state.catalog.job_counts(None).unwrap().failed, 1);

    // The bulk form previews first: counts and bytes, no state change.
    let preview = post(
        &rt,
        &app,
        &format!("/api/sources/{}/content/retry", sid.0),
        r#"{"preview":true}"#,
    );
    assert_eq!(preview["preview"], serde_json::Value::Bool(true));
    assert_eq!(n(&preview, "accepted"), 1);
    assert_eq!(n(&preview, "bytes"), TEXT.len() as u64);
    assert_eq!(preview["confirmation"].as_str().unwrap().len(), 64);
    assert_eq!((n(&preview, "skipped"), n(&preview, "rejected")), (0, 0));
    assert_eq!(state.catalog.job_counts(None).unwrap().failed, 1);

    // A class that did not fail matches nothing.
    let other = post(
        &rt,
        &app,
        &format!("/api/sources/{}/content/retry", sid.0),
        r#"{"preview":true,"class":"unsupported"}"#,
    );
    assert_eq!(n(&other, "accepted"), 0);

    // Retry the single job, twice: the second call cannot double-queue it.
    let first = post(&rt, &app, &format!("/api/jobs/{}/retry", job.id.0), "{}");
    assert_eq!(n(&first, "accepted"), 1);
    assert_eq!(n(&first, "bytes"), TEXT.len() as u64);
    let second = post(&rt, &app, &format!("/api/jobs/{}/retry", job.id.0), "{}");
    assert_eq!((n(&second, "accepted"), n(&second, "rejected")), (0, 1));
    assert_eq!(second["rejected_reasons"]["queued"], "1");
    let queued = state.catalog.job_counts(None).unwrap();
    assert_eq!((queued.queued, queued.running, queued.failed), (1, 0, 0));
    let record = state.catalog.get_job(job.id).unwrap().unwrap();
    assert_eq!(record.requeue_count, 1, "one requeue, not two");
    assert_eq!(record.attempts, 1, "attempt history survives the retry");

    // The workers pick the requeued job up and the text becomes searchable.
    spawn_content_workers(&state, 2);
    top_up_queue(&state).unwrap();
    assert!(
        wait_until(Duration::from_secs(30), || {
            state
                .catalog
                .source_completeness(sid)
                .map(|c| c.content_complete)
                .unwrap_or(false)
        }),
        "retried job was never processed: {:?}",
        state.content_workers.view(0)
    );
    eidos_service::follower::follow_once(&state).unwrap();
    state.index.reload().unwrap();
    let parsed = eidos_query::parse("content:qzendpoint").unwrap();
    let hits = search_with_content(
        &state.index,
        Some(&state.content_index),
        &state.catalog,
        &SearchRequest::new(parsed.query),
        &ExecOptions::default(),
    )
    .unwrap()
    .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "a.txt");

    // Nothing failed any more, and a finished job is rejected, not re-run.
    let empty = post(
        &rt,
        &app,
        &format!("/api/sources/{}/content/retry", sid.0),
        r#"{"preview":false}"#,
    );
    assert_eq!(n(&empty, "accepted"), 0);
    let done = post(&rt, &app, &format!("/api/jobs/{}/retry", job.id.0), "{}");
    assert_eq!((n(&done, "accepted"), n(&done, "rejected")), (0, 1));
    assert_eq!(state.content_workers.view(0).files_indexed, 1);

    // A deterministic failure lives on the content record with a finished
    // job. The bulk retry puts the object back to `pending` and the workers
    // extract it again.
    let object = job.object_id.expect("content job has an object");
    let record = state.catalog.content_record(object).unwrap().unwrap();
    state
        .catalog
        .finish_content(
            &eidos_catalog::content::ContentRecord {
                state: eidos_domain::ContentState::Failed,
                coverage: eidos_domain::Coverage::None,
                failure_class: Some(FailureClass::Deterministic),
                error: Some("extract: no decoder".into()),
                ..record
            },
            true,
        )
        .unwrap();
    let deterministic_preview = post(
        &rt,
        &app,
        &format!("/api/sources/{}/content/retry", sid.0),
        r#"{"class":"deterministic","preview":true}"#,
    );
    let confirmation = serde_json::json!({
        "class": "deterministic",
        "limit": n(&deterministic_preview, "accepted"),
        "as_of": deterministic_preview["as_of"],
        "confirmation": deterministic_preview["confirmation"],
    });
    let deterministic = post(
        &rt,
        &app,
        &format!("/api/sources/{}/content/retry", sid.0),
        &confirmation.to_string(),
    );
    assert_eq!(n(&deterministic, "accepted"), 1);
    assert_eq!(n(&deterministic, "bytes"), TEXT.len() as u64);
    assert!(
        wait_until(Duration::from_secs(30), || {
            state.content_workers.view(0).files_indexed >= 2
        }),
        "requeued object was never re-extracted: {:?}",
        state.content_workers.view(0)
    );
    assert_eq!(
        state
            .catalog
            .get_job(job.id)
            .unwrap()
            .unwrap()
            .requeue_count,
        2,
        "both operator actions are on the record"
    );

    // Unknown ids and unparseable filters are refused, not silently empty.
    assert_eq!(status(&rt, &app, "/api/jobs/9999/retry", "{}"), 404);
    assert_eq!(
        status(&rt, &app, "/api/sources/9999/content/retry", "{}"),
        404
    );
    assert_eq!(
        status(
            &rt,
            &app,
            &format!("/api/sources/{}/content/retry", sid.0),
            r#"{"class":"nonsense"}"#,
        ),
        400
    );
    state.request_shutdown();
}
