//! The catalog-index follower releases consumed notification-outbox rows on
//! idle iterations, so a backlog left by an older binary (which consumed
//! without pruning) drains without new writes — and once nothing is
//! prunable, an idle iteration takes no writer transaction at all, or the
//! follower's own prune would bump the write signal and wake it forever.

use eidos_catalog::changes::{ChangeEvent, ObjectSnapshot};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{SourceKind, UnixNanos};
use eidos_search::PROJECTION_NAME;
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::sync::Arc;

#[test]
fn idle_follower_drains_consumed_backlog() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("zephyr");
    std::fs::create_dir_all(root.join("reports")).unwrap();
    for i in 0..12 {
        std::fs::write(root.join(format!("reports/qz-{i:02}.log")), b"x\n").unwrap();
    }
    let state = Arc::new(
        AppState::open(&ServiceConfig {
            data_dir: dir.path().join("data"),
            web_dir: None,
            scan_threads: 1,
            auto_reconcile: false,
            content: false,
            content_workers: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let source = state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: "fixture".into(),
            kind: SourceKind::WindowsLocal,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    let scan = |state: &AppState| {
        run_scan(
            &state.catalog,
            source,
            state.lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
    };
    scan(&state);
    eidos_service::follower::follow_once(&state).unwrap();

    // Produce outbox rows through change application (scans publish by
    // generation swap and emit none), then behave like a pre-prune binary:
    // apply them to the index, record the position, mark them consumed,
    // never delete.
    for i in 0..12 {
        let id = state
            .catalog
            .resolve_relative(source, &format!("reports/qz-{i:02}.log"))
            .unwrap()
            .unwrap();
        let o = state.catalog.get_object(id).unwrap().unwrap();
        let snapshot = ObjectSnapshot {
            native: o.native.expect("native identity"),
            kind: o.kind,
            attributes: o.attributes,
            size: 3,
            allocated: 3,
            link_count: 1,
            created: o.created,
            modified: Some(UnixNanos(1_700_000_000_000_000_000 + i)),
            changed: Some(UnixNanos(1_700_000_000_000_000_000 + i)),
            accessed: o.accessed,
            reparse_tag: 0,
        };
        state
            .catalog
            .apply_changes(source, &[ChangeEvent::Update { snapshot }], None)
            .unwrap();
    }
    let rows = state.catalog.outbox_poll(0, 1_000).unwrap();
    assert!(
        !rows.is_empty(),
        "rescan of modified files emits outbox rows"
    );
    let last = rows.last().unwrap().seq;
    state.index.apply_outbox(&state.catalog, &rows).unwrap();
    state
        .catalog
        .set_projection_position(PROJECTION_NAME, last)
        .unwrap();
    state.catalog.outbox_consume(last).unwrap();
    assert_eq!(state.catalog.outbox_pending().unwrap(), 0);
    let leftover = state.catalog.outbox_retained().unwrap();
    assert!(leftover >= rows.len() as u64);

    // Nothing pending: this iteration is idle, and it still prunes.
    eidos_service::follower::follow_once(&state).unwrap();
    assert_eq!(state.catalog.outbox_pending().unwrap(), 0);
    assert!(
        state.catalog.outbox_retained().unwrap() < leftover,
        "idle iteration released consumed rows"
    );
    let mut guard = 0;
    while state.catalog.outbox_retained().unwrap() > 0 {
        eidos_service::follower::follow_once(&state).unwrap();
        guard += 1;
        assert!(guard < 100, "backlog did not drain");
    }

    // Fully drained: idle iterations must be read-only. A writer
    // transaction here would bump the write signal and turn the
    // event-driven follower into a self-waking busy loop (the v0.5.0
    // regression: ~2,200 writer acquisitions/s while idle).
    assert!(!state.catalog.outbox_has_prunable().unwrap());
    let before = state.catalog.writer_stats().acquisitions;
    for _ in 0..5 {
        eidos_service::follower::follow_once(&state).unwrap();
    }
    let after = state.catalog.writer_stats().acquisitions;
    assert_eq!(
        before, after,
        "an idle follower iteration with nothing to prune took the catalog writer"
    );
}
