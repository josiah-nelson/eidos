//! `GET /api/objects/{id}/content` driven through the real router.
//!
//! The preview serves the catalog's stored chunk text, so these tests write
//! chunks directly (rather than going through extraction) to pin down exactly
//! what the endpoint does with unicode, very long lines, control characters,
//! stale generations, missing chunks, and its own limits.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use eidos_catalog::content::ContentRecord;
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_content::Chunk;
use eidos_domain::{ContentState, Coverage, ObjectId, SourceId, SourceKind, UnixNanos};
use eidos_service::content_preview::{MAX_NEIGHBORS, MAX_RESPONSE_BYTES};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: Arc<AppState>,
    source: SourceId,
}

impl Env {
    /// A scanned source with one file per name, ready for stored chunks.
    fn new(files: &[&str]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        for f in files {
            std::fs::write(root.join(f), format!("placeholder for {f}\n")).unwrap();
        }
        let state = Arc::new(
            AppState::open(&ServiceConfig {
                data_dir: dir.path().join("data"),
                scan_threads: 2,
                auto_reconcile: false,
                content: false,
                content_workers: 0,
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
        let env = Env {
            _dir: dir,
            root,
            state,
            source,
        };
        env.scan();
        env
    }

    fn scan(&self) {
        run_scan(
            &self.state.catalog,
            self.source,
            self.state.lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
    }

    fn object(&self, name: &str) -> ObjectId {
        self.state
            .catalog
            .resolve_relative(self.source, name)
            .unwrap()
            .unwrap_or_else(|| panic!("{name} not in the catalog"))
    }

    fn generation(&self, object: ObjectId) -> u32 {
        self.state
            .catalog
            .get_object(object)
            .unwrap()
            .unwrap()
            .generation
    }

    /// Store `texts` as consecutive chunks of the object's current
    /// generation, with the byte and line ranges they would really have.
    fn store(&self, object: ObjectId, texts: &[String]) -> u32 {
        let generation = self.generation(object);
        let (mut byte, mut line) = (0u64, 0u64);
        let mut chunks = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            let lines = text.lines().count().max(1) as u64;
            chunks.push(Chunk {
                ordinal: i as u32,
                byte_start: byte,
                byte_end: byte + text.len() as u64,
                line_start: line,
                line_end: line + lines - 1,
                text: text.clone(),
                split_line: false,
            });
            byte += text.len() as u64;
            line += lines;
        }
        let rec = ContentRecord {
            object_id: object,
            source_id: self.source,
            generation,
            extraction_version: 1,
            encoding: Some("utf-8".into()),
            coverage: Coverage::Full,
            indexed_bytes: byte,
            total_bytes: byte,
            chunk_count: texts.len() as u32,
            line_count: line,
            chars: texts.iter().map(|t| t.chars().count() as u64).sum(),
            content_id: None,
            hash_complete: true,
            state: ContentState::Indexed,
            failure_class: None,
            error: None,
            reason: None,
            processed_at: UnixNanos::now(),
            elapsed_ms: 1.0,
        };
        self.state
            .catalog
            .store_content(&rec, &chunks, true, None)
            .unwrap();
        generation
    }

    fn router(&self) -> Router {
        eidos_service::api::router(self.state.clone(), None)
    }
}

async fn get(app: &Router, uri: String) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn chunks(v: &Value) -> &Vec<Value> {
    v["chunks"].as_array().expect("chunks array")
}

fn text(v: &Value, i: usize) -> &str {
    chunks(v)[i]["text"].as_str().unwrap()
}

#[tokio::test]
async fn serves_unicode_text_with_byte_and_line_metadata() {
    let e = Env::new(&["notes.txt"]);
    let oid = e.object("notes.txt");
    let bodies = vec![
        "первый chunk\nwith 漢字 and 😀\n".to_string(),
        "second — em dash, combining é\u{0301}\n".to_string(),
        "third\n".to_string(),
    ];
    let generation = e.store(oid, &bodies);
    let app = e.router();

    let (status, v) = get(
        &app,
        format!(
            "/api/objects/{}/content?generation={generation}&ordinal=1&before=1&after=1",
            oid.0
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["generation"], generation);
    assert_eq!(v["object_generation"], generation);
    assert_eq!(v["stale"], false);
    assert_eq!(v["state"], "indexed");
    assert_eq!(v["coverage"], "full");
    assert_eq!(v["chunk_count"], 3);
    assert_eq!(v["requested_ordinal"], 1);
    assert_eq!(v["truncated"], false);
    assert_eq!(v["has_more_before"], false);
    assert_eq!(v["has_more_after"], false);
    assert_eq!(v["limits"]["max_neighbors"], MAX_NEIGHBORS);
    assert!(v["path"].as_str().unwrap().ends_with("notes.txt"));
    assert_eq!(chunks(&v).len(), 3);
    for (i, want) in bodies.iter().enumerate() {
        assert_eq!(text(&v, i), want, "chunk {i} text must round-trip exactly");
        assert_eq!(chunks(&v)[i]["sanitized"], false);
        assert_eq!(chunks(&v)[i]["truncated"], false);
        assert_eq!(chunks(&v)[i]["chars"], want.chars().count() as u64);
    }
    // Ranges are the stored ones: byte ranges are contiguous, lines advance.
    assert_eq!(chunks(&v)[0]["byte_start"], 0);
    assert_eq!(chunks(&v)[1]["byte_start"], bodies[0].len() as u64);
    assert_eq!(chunks(&v)[0]["line_start"], 0);
    assert_eq!(chunks(&v)[1]["line_start"], 2);
    assert_eq!(chunks(&v)[2]["line_start"], 3);

    // Stored text is served even after the file is gone: the endpoint never
    // touches the source.
    std::fs::remove_file(e.root.join("notes.txt")).unwrap();
    let (status, v) = get(&app, format!("/api/objects/{}/content?ordinal=0", oid.0)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(text(&v, 0), bodies[0]);
}

#[tokio::test]
async fn replaces_control_characters() {
    let e = Env::new(&["hostile.txt"]);
    let oid = e.object("hostile.txt");
    let hostile =
        "safe\ttab\r\nline\u{0}nul\u{7}bell\u{1b}[31mred\u{7f}del\u{202e}reversed\u{9c}c1\n";
    e.store(oid, &[hostile.to_string()]);

    let (status, v) = get(&e.router(), format!("/api/objects/{}/content", oid.0)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let got = text(&v, 0);
    assert_eq!(chunks(&v)[0]["sanitized"], true);
    assert_eq!(
        got,
        "safe\ttab\r\nline\u{fffd}nul\u{fffd}bell\u{fffd}[31mred\u{fffd}del\u{fffd}reversed\u{fffd}c1\n"
    );
    assert!(
        !got.chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')),
        "no C0/C1 control other than tab/CR/LF may survive"
    );
    // One replacement per character: offsets into the stored text still line up.
    assert_eq!(got.chars().count(), hostile.chars().count());
    assert_eq!(chunks(&v)[0]["chars"], hostile.chars().count() as u64);
}

#[tokio::test]
async fn truncates_a_very_long_line() {
    let e = Env::new(&["huge.log"]);
    let oid = e.object("huge.log");
    let line = format!("{}\n", "x".repeat(400_000));
    e.store(oid, &[line]);

    let (status, v) = get(&e.router(), format!("/api/objects/{}/content", oid.0)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["truncated"], true);
    assert_eq!(chunks(&v)[0]["truncated"], true);
    assert_eq!(text(&v, 0).len(), MAX_RESPONSE_BYTES);
    // The stored chunk's own metadata is preserved, not the cut length.
    assert_eq!(chunks(&v)[0]["chars"], 400_001);
    assert_eq!(chunks(&v)[0]["byte_end"], 400_001);
}

#[tokio::test]
async fn rejects_a_stale_generation_and_flags_lagging_content() {
    let e = Env::new(&["moving.txt"]);
    let oid = e.object("moving.txt");
    let indexed = e.store(oid, &["text of the first version\n".to_string()]);
    let app = e.router();

    // A generation that was never stored is refused, with the current one.
    let (status, v) = get(
        &app,
        format!("/api/objects/{}/content?generation={}", oid.0, indexed + 7),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["kind"], "stale_generation");
    assert_eq!(v["requested_generation"], indexed + 7);
    assert_eq!(v["current_generation"], indexed);
    assert!(v["error"].as_str().unwrap().contains("generation"));

    // The file changes: the object moves on, the stored text does not.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(e.root.join("moving.txt"), b"a different, longer body\n").unwrap();
    e.scan();
    let current = e.generation(oid);
    assert!(current > indexed, "rescan must bump the object generation");

    let (status, v) = get(&app, format!("/api/objects/{}/content", oid.0)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["stale"], true, "stored text predates the object");
    assert_eq!(v["generation"], indexed);
    assert_eq!(v["object_generation"], current);
    assert_eq!(text(&v, 0), "text of the first version\n");

    // Asking for the object's new generation is a conflict, not silent text
    // from the old one.
    let (status, v) = get(
        &app,
        format!("/api/objects/{}/content?generation={current}", oid.0),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["current_generation"], indexed);
}

#[tokio::test]
async fn missing_objects_records_and_ordinals_are_not_found() {
    let e = Env::new(&["indexed.txt", "bare.txt"]);
    let oid = e.object("indexed.txt");
    e.store(oid, &["one\n".to_string(), "two\n".to_string()]);
    let app = e.router();

    let (status, v) = get(&app, format!("/api/objects/{}/content?ordinal=9", oid.0)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["kind"], "not_found");
    assert!(v["error"].as_str().unwrap().contains("chunk 9"));

    let bare = e.object("bare.txt");
    let (status, v) = get(&app, format!("/api/objects/{}/content", bare.0)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("no extracted content"));

    let (status, _) = get(&app, "/api/objects/999999/content".to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn clamps_neighbours_and_response_size() {
    let e = Env::new(&["wide.txt", "fat.txt"]);

    // Neighbour clamp: 12 small chunks, an unbounded request, a window of
    // MAX_NEIGHBORS per side around the requested ordinal.
    let wide = e.object("wide.txt");
    let small: Vec<String> = (0..12).map(|i| format!("chunk {i}\n")).collect();
    e.store(wide, &small);
    let app = e.router();
    let (status, v) = get(
        &app,
        format!(
            "/api/objects/{}/content?ordinal=6&before=99&after=99",
            wide.0
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ordinals: Vec<u64> = chunks(&v)
        .iter()
        .map(|c| c["ordinal"].as_u64().unwrap())
        .collect();
    assert_eq!(ordinals, vec![2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(v["has_more_before"], true);
    assert_eq!(v["has_more_after"], true);
    assert_eq!(
        v["truncated"], false,
        "clamping neighbours is not truncation"
    );

    // Byte cap: 40 KiB chunks, so the full neighbour window would be 360 KiB.
    let fat = e.object("fat.txt");
    let body: String = (0..40).map(|i| format!("{i:01023}\n")).collect();
    assert_eq!(body.len(), 40 * 1024);
    let fat_chunks: Vec<String> = (0..12).map(|_| body.clone()).collect();
    e.store(fat, &fat_chunks);
    let (status, v) = get(
        &app,
        format!("/api/objects/{}/content?ordinal=6&before=4&after=4", fat.0),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let total: usize = chunks(&v)
        .iter()
        .map(|c| c["text"].as_str().unwrap().len())
        .sum();
    assert!(
        total <= MAX_RESPONSE_BYTES,
        "{total} bytes of text returned"
    );
    assert!(chunks(&v).len() < 9, "the whole window cannot fit");
    assert!(
        chunks(&v).len() >= 3,
        "neighbours still fit: {}",
        chunks(&v).len()
    );
    assert_eq!(v["truncated"], true);
    // The requested chunk is always present and whole here.
    let center = chunks(&v)
        .iter()
        .find(|c| c["ordinal"] == 6)
        .expect("requested chunk");
    assert_eq!(center["truncated"], false);
    assert_eq!(center["text"].as_str().unwrap().len(), 40 * 1024);
}
