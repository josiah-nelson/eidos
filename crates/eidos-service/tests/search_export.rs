//! `/api/search/export`: RFC 4180 CSV, the versioned JSON/NDJSON envelope,
//! bounded cursor walks, and cancellation when the client goes away.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{SourceId, SourceKind, SourceState};
use eidos_service::export::ExportLimits;
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use http_body_util::BodyExt;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// A source name is catalog data, not a file name, so it is the one column
/// that can carry the characters NTFS forbids (quotes and line breaks).
const SOURCE_NAME: &str = "qz,\"east\"\r\nnorth";

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

fn env(build: impl FnOnce(&std::path::Path)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(&root).unwrap();
    build(&root);
    Env {
        data: dir.path().join("data"),
        _dir: dir,
        root,
    }
}

fn open_state(e: &Env, export: ExportLimits) -> Arc<AppState> {
    open_state_with(e, export, Default::default())
}

fn open_state_with(
    e: &Env,
    export: ExportLimits,
    admission: eidos_service::admission::AdmissionConfig,
) -> Arc<AppState> {
    let cfg = ServiceConfig {
        admission,
        data_dir: e.data.clone(),
        scan_threads: 2,
        auto_reconcile: false,
        content: false,
        content_workers: 0,
        export,
        ..Default::default()
    };
    Arc::new(AppState::open(&cfg).unwrap())
}

/// Scan the fixture and make it searchable. Content workers never run, so
/// every file stays `pending` — which is what the completeness test wants.
fn scan(state: &AppState, e: &Env) -> SourceId {
    let sid = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: SOURCE_NAME.into(),
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
    eidos_service::follower::follow_once(state).unwrap();
    state.index.reload().unwrap();
    sid
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Vec<(String, String)>, String) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Minimal RFC 4180 reader, so the assertions test the file as a parser sees
/// it rather than as a substring.
fn parse_csv(input: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' if field.is_empty() => quoted = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' if chars.peek() == Some(&'\n') => {
                    chars.next();
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

fn column(records: &[Vec<String>], name: &str) -> usize {
    records[0].iter().position(|c| c == name).unwrap()
}

#[tokio::test]
async fn csv_quotes_commas_quotes_newlines_and_unicode() {
    let e = env(|root| {
        std::fs::write(root.join("plain.txt"), b"x").unwrap();
        std::fs::write(root.join("a,b.txt"), b"x").unwrap();
        std::fs::write(root.join("naïve — Ωμέγα 漢字.txt"), b"x").unwrap();
    });
    let state = open_state(&e, ExportLimits::default());
    scan(&state, &e);
    let app = eidos_service::api::router(state.clone(), None);

    let (status, headers, body) =
        get(&app, "/api/search/export?format=csv&q=ext:txt&sort=name").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(header(&headers, "x-eidos-export-total"), Some("3"));
    assert_eq!(header(&headers, "x-eidos-export-max-rows"), Some("100000"));
    assert!(!body.starts_with('\u{feff}'), "no BOM unless asked for");

    let records = parse_csv(&body);
    assert_eq!(
        records[0],
        eidos_service::export::COLUMNS
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(records.len(), 4, "header + 3 rows");
    let (name_col, source_col) = (column(&records, "name"), column(&records, "source"));
    let names: Vec<&str> = records[1..].iter().map(|r| r[name_col].as_str()).collect();
    assert!(names.contains(&"a,b.txt"), "{names:?}");
    assert!(names.contains(&"naïve — Ωμέγα 漢字.txt"), "{names:?}");
    // Comma, doubled quote, and a CRLF all survive one round trip.
    assert!(records[1..].iter().all(|r| r[source_col] == SOURCE_NAME));
    assert!(body.contains("\"qz,\"\"east\"\"\r\nnorth\""));

    // Timestamps: RFC 3339 UTC, full nanosecond field, never rounded.
    let modified = column(&records, "modified");
    for r in &records[1..] {
        let t = &r[modified];
        assert!(t.ends_with('Z') && t.len() == 30, "{t}");
        assert_eq!(t.as_bytes()[19], b'.', "{t}");
    }

    let (_, _, bom) = get(&app, "/api/search/export?format=csv&q=ext:txt&bom=1").await;
    assert!(bom.starts_with('\u{feff}'), "bom=1 prefixes a UTF-8 BOM");
}

#[tokio::test]
async fn walks_every_page_without_duplicates_and_marks_truncation() {
    let e = env(|root| {
        for i in 0..1200 {
            std::fs::write(root.join(format!("f{i:04}.txt")), b"x").unwrap();
        }
        for i in 0..120 {
            std::fs::write(root.join(format!("n{i:03}.md")), b"x").unwrap();
        }
    });
    let state = open_state(
        &e,
        ExportLimits {
            page_size: 100,
            max_rows: 500,
        },
    );
    scan(&state, &e);
    let app = eidos_service::api::router(state.clone(), None);

    // 120 rows over two cursor steps: complete, in order, no repeats.
    let before = state.export_stats.pages.load(Ordering::Relaxed);
    let (_, _, md) = get(&app, "/api/search/export?format=csv&q=ext:md&sort=name").await;
    let records = parse_csv(&md);
    assert_eq!(records.len(), 121);
    let ids = column(&records, "object_id");
    let mut seen: Vec<&str> = records[1..].iter().map(|r| r[ids].as_str()).collect();
    let total = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), total, "cursor walk repeated a row");
    assert_eq!(
        state.export_stats.pages.load(Ordering::Relaxed) - before,
        2,
        "120 rows at 100 per page"
    );

    // The service cap wins over the 1200 matches.
    let (_, headers, capped) =
        get(&app, "/api/search/export?format=ndjson&q=ext:txt&sort=name").await;
    assert_eq!(header(&headers, "x-eidos-export-total"), Some("1200"));
    assert_eq!(header(&headers, "x-eidos-export-max-rows"), Some("500"));
    let lines: Vec<&str> = capped.lines().collect();
    assert_eq!(lines.len(), 502, "header + 500 rows + summary");
    let summary: serde_json::Value = serde_json::from_str(lines[501]).unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["rows_exported"], 500);
    assert_eq!(summary["truncated"], true);
    assert!(summary["error"].is_null());

    // A per-request limit below the cap truncates too.
    let (_, headers, small) =
        get(&app, "/api/search/export?format=ndjson&q=ext:txt&limit=250").await;
    assert_eq!(header(&headers, "x-eidos-export-max-rows"), Some("250"));
    let lines: Vec<&str> = small.lines().collect();
    assert_eq!(lines.len(), 252);
    let summary: serde_json::Value = serde_json::from_str(lines[251]).unwrap();
    assert_eq!(summary["truncated"], true);
}

#[tokio::test]
async fn envelope_carries_completeness_of_stale_and_partial_sources() {
    let e = env(|root| {
        std::fs::write(root.join("a.txt"), b"zephyr").unwrap();
        std::fs::write(root.join("b.txt"), b"zephyr").unwrap();
    });
    let state = open_state(&e, ExportLimits::default());
    let sid = scan(&state, &e);
    // No content workers ran, so content is pending; mark the source stale on
    // top of that so metadata completeness is false as well.
    state
        .catalog
        .set_source_state(sid, SourceState::Stale, Some("feed lost"))
        .unwrap();
    let app = eidos_service::api::router(state.clone(), None);

    let (status, _, body) = get(&app, "/api/search/export?format=json&q=ext:txt").await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["schema"], "eidos-export/1");
    let c = &doc["completeness"][0];
    assert_eq!(c["state"], "stale");
    assert_eq!(c["metadata_complete"], false);
    assert_eq!(c["content_complete"], false);
    assert_eq!(c["content_pending"], 2);
    assert_eq!(c["freshness"], "unknown");
    assert_eq!(c["note"], "feed lost");
    assert_eq!(doc["rows_exported"], 2);
    assert_eq!(doc["truncated"], false);
}

#[tokio::test]
async fn json_and_ndjson_share_one_versioned_row_schema() {
    let e = env(|root| {
        std::fs::write(root.join("a.txt"), b"x").unwrap();
        std::fs::write(root.join("b.txt"), b"x").unwrap();
    });
    let state = open_state(&e, ExportLimits::default());
    scan(&state, &e);
    let app = eidos_service::api::router(state.clone(), None);

    let (_, headers, body) = get(&app, "/api/search/export?format=json&q=ext:txt&sort=name").await;
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/json; charset=utf-8")
    );
    assert!(header(&headers, "content-disposition")
        .is_some_and(|d| d.starts_with("attachment; filename=\"eidos-export-")));
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["schema"], "eidos-export/1");
    assert!(doc["query"]["rendered"].as_str().unwrap().contains("txt"));
    assert_eq!(doc["query"]["mode"], "files");
    assert!(doc["query"]["ast"].is_object());
    assert!(doc["exported_at"].as_str().unwrap().ends_with('Z'));
    assert!(doc["warnings"].is_array());
    let rows = doc["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let mut keys: Vec<&str> = rows[0]
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    let mut expected: Vec<&str> = eidos_service::export::COLUMNS.to_vec();
    keys.sort_unstable();
    expected.sort_unstable();
    assert_eq!(keys, expected, "JSON rows carry exactly the CSV columns");
    // Absent values stay in the row as explicit nulls (see the unit test in
    // `export.rs`); present ones keep their type.
    assert!(rows[0]["score"].is_number() || rows[0]["score"].is_null());
    assert!(rows[0]["path"].is_string());
    assert!(rows[0]["modified"].as_str().unwrap().ends_with('Z'));

    let (_, _, nd) = get(&app, "/api/search/export?format=ndjson&q=ext:txt&sort=name").await;
    let lines: Vec<&str> = nd.lines().collect();
    assert_eq!(lines.len(), 4);
    let head: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(head["schema"], "eidos-export/1");
    assert_eq!(head["type"], "header");
    assert!(head["completeness"].is_array());
    let row: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(row, rows[0], "NDJSON rows equal the JSON document's rows");

    // POST takes the same query body as /api/search plus the format.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/search/export")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"q":"ext:txt","format":"ndjson","limit":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert_eq!(text.lines().count(), 3, "header + 1 row + summary");

    // A bad query is a normal JSON error, not a truncated stream.
    let (status, _, err) = get(&app, "/api/search/export?format=csv&q=name%3A%2F%5B%2F").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err.contains("\"kind\""), "{err}");
}

#[tokio::test]
async fn dropping_the_response_body_stops_the_walk() {
    let e = env(|root| {
        for i in 0..1200 {
            std::fs::write(root.join(format!("f{i:04}.txt")), b"x").unwrap();
        }
    });
    let state = open_state(
        &e,
        ExportLimits {
            page_size: 5,
            max_rows: 100_000,
        },
    );
    scan(&state, &e);
    let app = eidos_service::api::router(state.clone(), None);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/search/export?format=csv&q=ext:txt&sort=name")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = resp.into_body();
    // One frame read, then the consumer goes away.
    let first = body.frame().await.unwrap().unwrap();
    assert!(!first.into_data().unwrap().is_empty());
    drop(body);

    let settled = wait_for_stable(&state).await;
    assert!(
        settled < 240,
        "the walk kept going after the body was dropped: {settled} of 240 pages"
    );
    assert_eq!(state.export_stats.cancelled.load(Ordering::Relaxed), 1);
    assert_eq!(state.export_stats.finished.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn client_disconnect_stops_the_walk() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let e = env(|root| {
        for i in 0..1200 {
            std::fs::write(root.join(format!("f{i:04}.txt")), b"x").unwrap();
        }
    });
    let state = open_state(
        &e,
        ExportLimits {
            page_size: 5,
            max_rows: 100_000,
        },
    );
    scan(&state, &e);
    let app = eidos_service::api::router(state.clone(), None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"GET /api/search/export?format=csv&q=ext:txt&sort=name HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut buf = [0u8; 1024];
    let n = sock.read(&mut buf).await.unwrap();
    assert!(n > 0, "no response bytes");
    drop(sock);

    let settled = wait_for_stable(&state).await;
    assert!(
        settled < 240,
        "the walk kept going after the client hung up: {settled} of 240 pages"
    );
    assert_eq!(state.export_stats.cancelled.load(Ordering::Relaxed), 1);
    server.abort();
}

/// Wait until the page counter stops moving, then return it.
async fn wait_for_stable(state: &AppState) -> u64 {
    let mut last = u64::MAX;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = state.export_stats.pages.load(Ordering::Relaxed);
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

/// The export takes an admission permit per page, not one for the whole
/// stream, so a parked export never locks interactive search out of the gate.
#[tokio::test]
async fn an_open_export_does_not_hold_the_admission_gate() {
    let e = env(|root| {
        for i in 0..600 {
            std::fs::write(root.join(format!("f{i:04}.txt")), b"x").unwrap();
        }
    });
    let state = open_state_with(
        &e,
        ExportLimits {
            page_size: 5,
            max_rows: 100_000,
        },
        eidos_service::admission::AdmissionConfig {
            // One permit: a stream-long permit would shut everything else out.
            concurrency: 1,
            ..Default::default()
        },
    );
    scan(&state, &e);
    let app = eidos_service::api::router(state.clone(), None);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/search/export?format=csv&q=ext:txt&sort=name")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = resp.into_body();
    body.frame().await.unwrap().unwrap();
    // The export is now parked between pages, holding no permit.
    let (status, _, _) = get(&app, "/api/search?q=ext:txt&limit=1").await;
    assert_eq!(status, StatusCode::OK, "search was shed behind the export");
    drop(body);
}
