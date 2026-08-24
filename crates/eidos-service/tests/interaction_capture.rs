//! `POST /api/interactions` over the real router: a batch reaches the catalog,
//! an oversize batch is refused, and capture never touches search results.

use eidos_catalog::interactions::{InteractionAction, InteractionRetention, QueryShape};
use eidos_service::interactions_api::MAX_INTERACTION_BATCH;
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

fn state(data: &std::path::Path) -> Arc<AppState> {
    Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: data.to_path_buf(),
            scan_threads: 1,
            auto_reconcile: false,
            content: false,
            content_workers: 1,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn post(
    rt: &tokio::runtime::Runtime,
    app: &axum::Router,
    uri: &str,
    body: &str,
) -> (u16, serde_json::Value) {
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
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "{uri} -> {status}: {e}: {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, value)
    })
}

/// The write is detached from the response, so tests wait for it.
fn wait_for_events(state: &AppState, expected: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = state.catalog.interaction_stats().unwrap().events;
        if events >= expected || Instant::now() > deadline {
            return events;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn events(session: &str, q: &str, count: usize) -> String {
    let rows: Vec<String> = (0..count)
        .map(|rank| {
            format!(
                r#"{{"session_id":"{session}","action":"presented","q":"{q}","object_id":"{}","source_id":"1","presented_rank":{rank}}}"#,
                1000 + rank
            )
        })
        .collect();
    format!(r#"{{"events":[{}]}}"#, rows.join(","))
}

#[test]
fn a_posted_batch_is_stored_and_an_oversize_batch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let state = state(&dir.path().join("data"));
    let app = eidos_service::api::router(state.clone(), None);

    let (status, ack) = post(
        &rt,
        &app,
        "/api/interactions",
        &events("tab-1", "ext:md", 3),
    );
    assert_eq!(status, 200);
    assert_eq!(ack["accepted"], 3);
    assert_eq!(ack["dropped"], 0);
    assert_eq!(wait_for_events(&state, 3), 3);

    // A second session, a content query, and the other actions.
    let (status, ack) = post(
        &rt,
        &app,
        "/api/interactions",
        r#"{"events":[
            {"session_id":"tab-2","action":"opened_preview","q":"content:zephyr","object_id":"77"},
            {"session_id":"tab-2","action":"copied_path","q":"content:zephyr","object_id":"77"},
            {"session_id":"tab-2","action":"exported","q":"content:zephyr"}
        ]}"#,
    );
    assert_eq!((status, ack["accepted"].as_u64()), (200, Some(3)));
    assert_eq!(wait_for_events(&state, 6), 6);

    let stats = state.catalog.interaction_stats().unwrap();
    assert_eq!((stats.events, stats.sessions), (6, 2));

    // What landed: shapes and actions in arrival order, and a digest in place
    // of every query.
    let rows = state.catalog.recent_interactions(100).unwrap();
    let actions: Vec<_> = rows.iter().map(|r| r.action).collect();
    assert_eq!(
        actions,
        [
            InteractionAction::Presented,
            InteractionAction::Presented,
            InteractionAction::Presented,
            InteractionAction::OpenedPreview,
            InteractionAction::CopiedPath,
            InteractionAction::Exported
        ]
    );
    assert_eq!(rows[0].query_shape, QueryShape::Metadata);
    assert_eq!(rows[3].query_shape, QueryShape::ContentRanked);
    assert_ne!(rows[0].query_hash, rows[3].query_hash);
    assert!(rows.iter().all(|r| r.query_hash.len() == 32));
    assert_eq!(rows[0].object_id, Some(eidos_domain::ObjectId(1000)));
    assert_eq!(rows[0].presented_rank, Some(0));
    assert_eq!(rows[5].object_id, None);

    // An oversize batch is refused with the API's error envelope, and nothing
    // from it is written.
    let (status, body) = post(
        &rt,
        &app,
        "/api/interactions",
        &events("tab-3", "ext:md", MAX_INTERACTION_BATCH + 1),
    );
    assert_eq!(status, 400);
    assert_eq!(body["kind"], "bad_request");
    assert!(
        body["error"].as_str().unwrap().contains("501"),
        "{body} should name the batch size"
    );

    // So is an unknown action; the batch is refused whole.
    let (status, body) = post(
        &rt,
        &app,
        "/api/interactions",
        r#"{"events":[{"session_id":"tab-3","action":"presented"},{"session_id":"tab-3","action":"clicked"}]}"#,
    );
    assert_eq!(
        (status, &body["kind"]),
        (400, &serde_json::json!("bad_request"))
    );
    // Validation runs before the write is handed off, so a refused batch has
    // provably written nothing by the time the response is out.
    assert_eq!(
        state.catalog.interaction_stats().unwrap().events,
        6,
        "a refused batch wrote rows"
    );

    // The largest accepted batch is exactly the cap.
    let (status, ack) = post(
        &rt,
        &app,
        "/api/interactions",
        &events("tab-4", "ext:md", MAX_INTERACTION_BATCH),
    );
    assert_eq!(status, 200);
    assert_eq!(ack["accepted"], MAX_INTERACTION_BATCH as u64);
    assert_eq!(wait_for_events(&state, 506), 506);
}

#[test]
fn a_restart_prunes_events_past_their_retention_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    {
        let state = state(&data);
        let app = eidos_service::api::router(state.clone(), None);
        post(
            &rt,
            &app,
            "/api/interactions",
            &events("tab-1", "ext:md", 4),
        );
        assert_eq!(wait_for_events(&state, 4), 4);
        // Age two of them past the window behind the endpoint's back; only a
        // clock that moved 90 days could do this through the API.
        state
            .catalog
            .with_writer(|conn| {
                let cutoff = eidos_domain::UnixNanos::now().0
                    - InteractionRetention::default().max_age.as_nanos() as i64
                    - 1;
                conn.execute(
                    "UPDATE interaction_events SET ts = ?1 WHERE presented_rank < 2",
                    [cutoff],
                )?;
                Ok(())
            })
            .unwrap();
    }
    let restarted = state(&data);
    assert_eq!(
        restarted.catalog.interaction_stats().unwrap().events,
        2,
        "startup did not enforce interaction retention"
    );
}
