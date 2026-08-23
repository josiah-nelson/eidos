//! Admission control for expensive HTTP operations: bounded concurrency,
//! bounded queueing, per-operation timeouts, body limits, and the accounting
//! that proves permits are released by the blocking work rather than by the
//! HTTP response.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{ObjectId, SourceId, SourceKind};
use eidos_service::admission::AdmissionConfig;
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

// ----- fixture -------------------------------------------------------------

struct Env {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    source: SourceId,
    root_object: ObjectId,
}

/// A small synthetic tree, scanned and indexed, so the gated handlers run
/// real catalog and index work.
fn env(admission: AdmissionConfig) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("zephyr");
    std::fs::create_dir_all(root.join("reports")).unwrap();
    for i in 0..24 {
        std::fs::write(
            root.join(format!("reports/qzendpoint-{i:02}.log")),
            format!("line {i}\n"),
        )
        .unwrap();
    }
    for i in 0..8 {
        std::fs::write(root.join(format!("note-{i}.md")), b"marker\n").unwrap();
    }
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.path().join("data"),
            web_dir: None,
            scan_threads: 2,
            auto_reconcile: false,
            content: false,
            content_workers: 1,
            admission,
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
    run_scan(
        &state.catalog,
        source,
        state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    eidos_service::follower::follow_once(&state).unwrap();
    state.index.reload().unwrap();
    let root_object = state
        .catalog
        .get_source(source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    Env {
        _dir: dir,
        state,
        source,
        root_object,
    }
}

fn router(env: &Env) -> axum::Router {
    eidos_service::api::router(env.state.clone(), None)
}

struct Res {
    status: StatusCode,
    retry_after: Option<String>,
    body: serde_json::Value,
}

impl Res {
    fn kind(&self) -> &str {
        self.body.get("kind").and_then(|k| k.as_str()).unwrap_or("")
    }
}

async fn call(app: &axum::Router, req: Request<Body>) -> Res {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let retry_after = res
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Res {
        status,
        retry_after,
        body,
    }
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_json(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn search_body(q: &str) -> String {
    serde_json::json!({ "q": q, "limit": 5 }).to_string()
}

async fn admission_view(app: &axum::Router) -> serde_json::Value {
    call(app, get("/api/index")).await.body["admission"].clone()
}

fn gauge(view: &serde_json::Value, field: &str) -> u64 {
    view[field]
        .as_u64()
        .unwrap_or_else(|| panic!("no {field} in {view}"))
}

// ----- test hook -----------------------------------------------------------

/// Holds every admitted operation of one class inside the blocking pool until
/// the test opens the gate, which is how a slow query is simulated without a
/// corpus large enough to be slow on its own.
#[derive(Clone)]
struct Gate {
    entered: Arc<AtomicUsize>,
    open: Arc<AtomicBool>,
}

impl Gate {
    fn install(state: &AppState, op: &'static str) -> Gate {
        let gate = Gate {
            entered: Arc::new(AtomicUsize::new(0)),
            open: Arc::new(AtomicBool::new(false)),
        };
        let (entered, open) = (gate.entered.clone(), gate.open.clone());
        state
            .admission
            .set_blocking_hook(Some(Arc::new(move |label| {
                if label != op {
                    return;
                }
                entered.fetch_add(1, Ordering::SeqCst);
                let start = Instant::now();
                while !open.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(30) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            })));
        gate
    }

    async fn wait_entered(&self, n: usize) {
        let start = Instant::now();
        while self.entered.load(Ordering::SeqCst) < n {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "only {} of {n} operations reached the blocking pool",
                self.entered.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn release(&self, state: &AppState) {
        state.admission.set_blocking_hook(None);
        self.open.store(true, Ordering::SeqCst);
    }
}

async fn wait_for(mut f: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ----- tests ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_search_sheds_load_while_health_stays_responsive() {
    let e = env(AdmissionConfig {
        concurrency: 1,
        queue_depth: 0,
        queue_wait: Duration::from_millis(50),
        search_timeout: Duration::from_secs(30),
        ..Default::default()
    });
    let app = router(&e);
    let gate = Gate::install(&e.state, "search");

    // One search occupies the only permit.
    let held = tokio::spawn({
        let app = app.clone();
        async move { call(&app, post_json("/api/search", search_body("marker"))).await }
    });
    gate.wait_entered(1).await;

    // The next one is shed immediately: the queue holds nothing.
    let shed = call(&app, post_json("/api/search", search_body("marker"))).await;
    assert_eq!(shed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(shed.kind(), "busy");
    let retry: u64 = shed
        .retry_after
        .as_deref()
        .expect("Retry-After on a shed request")
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry), "retry-after {retry}");

    // Health must not wait behind the saturated gate.
    let t = Instant::now();
    let health = call(&app, get("/api/health")).await;
    assert_eq!(health.status, StatusCode::OK);
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "health took {:?} under a saturated limiter",
        t.elapsed()
    );
    // So must the status endpoints that report the saturation.
    let view = admission_view(&app).await;
    assert_eq!(gauge(&view, "in_flight"), 1);
    assert_eq!(gauge(&view, "rejected_busy"), 1);
    assert_eq!(gauge(&view, "limit"), 1);

    gate.release(&e.state);
    assert_eq!(held.await.unwrap().status, StatusCode::OK);
    wait_for(
        || e.state.admission.view().in_flight == 0,
        "permit released after the held search finished",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_requests_wait_then_shed_with_retry_after() {
    let e = env(AdmissionConfig {
        concurrency: 1,
        queue_depth: 4,
        queue_wait: Duration::from_millis(150),
        ..Default::default()
    });
    let app = router(&e);
    let gate = Gate::install(&e.state, "search");
    let held = tokio::spawn({
        let app = app.clone();
        async move { call(&app, post_json("/api/search", search_body("marker"))).await }
    });
    gate.wait_entered(1).await;

    // Queue depth is 4, so this one waits rather than being shed at once,
    // and gives up after the bounded wait.
    let t = Instant::now();
    let queued = call(&app, post_json("/api/search", search_body("marker"))).await;
    assert!(
        t.elapsed() >= Duration::from_millis(150),
        "queued request returned after {:?}, before the wait bound",
        t.elapsed()
    );
    assert_eq!(queued.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(queued.kind(), "busy");
    assert!(queued.retry_after.is_some());
    assert_eq!(e.state.admission.view().queued, 0, "queue gauge drained");

    gate.release(&e.state);
    assert_eq!(held.await.unwrap().status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_answers_504_and_the_permit_follows_the_blocking_work() {
    let e = env(AdmissionConfig {
        concurrency: 1,
        queue_depth: 0,
        queue_wait: Duration::from_millis(10),
        search_timeout: Duration::from_millis(150),
        ..Default::default()
    });
    let app = router(&e);
    let gate = Gate::install(&e.state, "search");

    let timed_out = call(&app, post_json("/api/search", search_body("marker"))).await;
    assert_eq!(timed_out.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(timed_out.kind(), "timeout");
    assert!(timed_out.retry_after.is_some());

    // The response is over but the query is not: the permit is still held, so
    // the gate is still full and the work is counted as detached.
    let view = admission_view(&app).await;
    assert_eq!(gauge(&view, "in_flight"), 1);
    assert_eq!(gauge(&view, "detached"), 1);
    assert_eq!(gauge(&view, "timed_out"), 1);
    let shed = call(&app, post_json("/api/search", search_body("marker"))).await;
    assert_eq!(shed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(shed.kind(), "busy");

    // Once the abandoned query finishes, every permit comes back.
    gate.release(&e.state);
    wait_for(
        || {
            let v = e.state.admission.view();
            v.in_flight == 0 && v.detached == 0
        },
        "permits and gauges return to zero after abandoned work drains",
    )
    .await;
    let v = e.state.admission.view();
    assert_eq!(v.admitted, v.completed, "no permit leaked: {v:?}");

    let ok = call(&app, post_json("/api/search", search_body("marker"))).await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_request_keeps_its_permit_until_the_work_finishes() {
    let e = env(AdmissionConfig {
        concurrency: 1,
        queue_depth: 0,
        queue_wait: Duration::from_millis(10),
        search_timeout: Duration::from_secs(30),
        ..Default::default()
    });
    let app = router(&e);
    let gate = Gate::install(&e.state, "search");

    // A client that goes away drops the handler future well before the
    // deadline; the query behind it is still running.
    let dropped = tokio::time::timeout(
        Duration::from_millis(200),
        call(&app, post_json("/api/search", search_body("marker"))),
    )
    .await;
    assert!(dropped.is_err(), "the request should not have completed");
    gate.wait_entered(1).await;

    let view = admission_view(&app).await;
    assert_eq!(gauge(&view, "in_flight"), 1);
    assert_eq!(gauge(&view, "detached"), 1, "disconnect counts as detached");
    assert_eq!(gauge(&view, "timed_out"), 0, "no deadline was reached");

    gate.release(&e.state);
    wait_for(
        || {
            let v = e.state.admission.view();
            v.in_flight == 0 && v.detached == 0 && v.admitted == v.completed
        },
        "abandoned work releases its permit and clears the gauges",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_request_body_is_rejected() {
    let e = env(AdmissionConfig {
        max_body_bytes: 2048,
        ..Default::default()
    });
    let app = router(&e);

    let small = call(&app, post_json("/api/search", search_body("marker"))).await;
    assert_eq!(small.status, StatusCode::OK);

    let huge = search_body(&"z".repeat(8192));
    let rejected = call(&app, post_json("/api/search", huge)).await;
    assert_eq!(rejected.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Nothing was admitted: an oversized body never reaches the gate.
    assert_eq!(e.state.admission.view().admitted, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broad_regex_counts_and_pagination_under_load_stay_bounded() {
    let e = env(AdmissionConfig {
        concurrency: 2,
        queue_depth: 4,
        queue_wait: Duration::from_millis(500),
        search_timeout: Duration::from_secs(20),
        operation_timeout: Duration::from_secs(20),
        ..Default::default()
    });
    let app = router(&e);
    let root = e.root_object.0;

    let mut tasks = Vec::new();
    for page in 0..6u32 {
        let regex_app = app.clone();
        // Broad regex over every indexed name, plus simultaneous pagination.
        tasks.push(tokio::spawn(async move {
            call(
                &regex_app,
                post_json(
                    "/api/search",
                    serde_json::json!({
                        "q": "name:/qz.*-\\d+/",
                        "limit": 4,
                        "mode": "files",
                    })
                    .to_string(),
                ),
            )
            .await
            .status
        }));
        let page_app = app.clone();
        tasks.push(tokio::spawn(async move {
            call(
                &page_app,
                get(&format!(
                    "/api/objects/{root}/children?offset={}&limit=4",
                    page * 4
                )),
            )
            .await
            .status
        }));
        let counts_app = app.clone();
        tasks.push(tokio::spawn(async move {
            call(&counts_app, get(&format!("/api/objects/{root}/extensions")))
                .await
                .status
        }));
    }
    for t in tasks {
        let status = t.await.unwrap();
        assert!(
            status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
            "unexpected status under load: {status}"
        );
    }

    wait_for(
        || {
            let v = e.state.admission.view();
            v.in_flight == 0 && v.queued == 0
        },
        "gate drains after the burst",
    )
    .await;
    let v = e.state.admission.view();
    assert_eq!(v.admitted, v.completed, "{v:?}");
    assert_eq!(v.timed_out, 0, "{v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_and_index_report_admission_counters() {
    let e = env(AdmissionConfig {
        concurrency: 3,
        queue_depth: 7,
        ..Default::default()
    });
    let app = router(&e);
    // A source keeps /api/activity doing real work.
    assert_eq!(e.source, SourceId(1));

    for uri in ["/api/index", "/api/activity"] {
        let res = call(&app, get(uri)).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}");
        let a = &res.body["admission"];
        assert_eq!(gauge(a, "limit"), 3, "{uri}");
        assert_eq!(gauge(a, "queue_depth"), 7, "{uri}");
        for field in [
            "in_flight",
            "queued",
            "detached",
            "admitted",
            "completed",
            "rejected_busy",
            "timed_out",
            "queue_wait_ms",
            "search_timeout_ms",
            "operation_timeout_ms",
            "max_body_bytes",
        ] {
            assert!(a.get(field).is_some(), "{uri} missing admission.{field}");
        }
    }
}

// ----- shutdown under load -------------------------------------------------

async fn raw_request(addr: SocketAddr, request: String) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

fn raw_search(body: &str) -> String {
    format!(
        "POST /api/search HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_shuts_down_cleanly_while_a_search_is_running() {
    let e = env(AdmissionConfig {
        concurrency: 1,
        queue_depth: 4,
        queue_wait: Duration::from_millis(200),
        search_timeout: Duration::from_secs(20),
        ..Default::default()
    });
    let app = router(&e);
    let gate = Gate::install(&e.state, "search");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
    });

    let body = search_body("marker");
    let client = tokio::spawn(raw_request(addr, raw_search(&body)));
    gate.wait_entered(1).await;

    // Shutdown while the search still holds its permit.
    e.state.request_shutdown();
    tx.send(()).unwrap();
    gate.release(&e.state);

    let response = tokio::time::timeout(Duration::from_secs(20), client)
        .await
        .expect("in-flight search never answered")
        .unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "in-flight search was dropped by shutdown: {}",
        response.lines().next().unwrap_or_default()
    );
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("server did not exit under load")
        .unwrap()
        .unwrap();

    let v = e.state.admission.view();
    assert_eq!(v.in_flight, 0, "{v:?}");
    assert_eq!(v.admitted, v.completed, "{v:?}");
}
