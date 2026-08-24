//! Interaction capture: batches land in arrival order, and the table's growth
//! is bounded by both the age limit and the row cap.

use eidos_catalog::interactions::{
    InteractionAction, InteractionEvent, InteractionRetention, QueryShape, MAX_INTERACTION_ROWS,
};
use eidos_catalog::Catalog;
use eidos_domain::{ObjectId, SourceId, UnixNanos};
use std::sync::Arc;
use std::time::Duration;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn catalog() -> (tempfile::TempDir, Arc<Catalog>) {
    let dir = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    (dir, catalog)
}

fn event(rank: u32, ts: UnixNanos, action: InteractionAction) -> InteractionEvent {
    InteractionEvent {
        ts,
        query_hash: eidos_catalog::interactions::query_hash("ext:md readme"),
        query_shape: QueryShape::Name,
        object_id: Some(ObjectId(1000 + i64::from(rank))),
        source_id: Some(SourceId(1)),
        presented_rank: Some(rank),
        action,
        session_id: "session-aaaa".into(),
    }
}

/// A stored row, read back as the table holds it rather than through the
/// typed accessor: these tests are about what reaches SQLite.
struct Row {
    id: i64,
    ts: i64,
    query_hash: String,
    query_shape: String,
    object_id: Option<i64>,
    presented_rank: Option<u32>,
    action: String,
}

/// Every stored row, oldest id first.
fn rows(catalog: &Catalog) -> Vec<Row> {
    catalog
        .with_reader(|conn| {
            Ok(conn
                .prepare(
                    "SELECT id, ts, query_hash, query_shape, object_id, presented_rank, action
                     FROM interaction_events ORDER BY id",
                )?
                .query_map([], |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        query_hash: r.get(2)?,
                        query_shape: r.get(3)?,
                        object_id: r.get(4)?,
                        presented_rank: r.get(5)?,
                        action: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap()
}

#[test]
fn a_batch_is_stored_in_arrival_order_with_every_column() {
    let (_dir, catalog) = catalog();
    let now = UnixNanos::now();
    let batch: Vec<_> = (0..5)
        .map(|rank| event(rank, now, InteractionAction::Presented))
        .chain([InteractionEvent {
            object_id: Some(ObjectId(1002)),
            presented_rank: Some(2),
            ..event(2, now, InteractionAction::OpenedPreview)
        }])
        .collect();
    assert_eq!(catalog.record_interactions(&batch).unwrap(), 6);

    let stored = rows(&catalog);
    assert_eq!(stored.len(), 6);
    // Ids are assigned in arrival order, so the sequence of a session reads
    // back exactly as it happened.
    assert!(stored.windows(2).all(|w| w[0].id < w[1].id));
    let ranks: Vec<_> = stored.iter().map(|r| r.presented_rank).collect();
    assert_eq!(
        ranks,
        vec![Some(0), Some(1), Some(2), Some(3), Some(4), Some(2)]
    );
    let actions: Vec<_> = stored.iter().map(|r| r.action.as_str()).collect();
    assert_eq!(actions[..5], ["presented"; 5]);
    assert_eq!(actions[5], "opened_preview");
    assert_eq!(stored[0].ts, now.0);
    assert_eq!(stored[0].query_shape, "name");
    assert_eq!(stored[0].object_id, Some(1000));
    // The digest, not the text, is what the row keeps.
    assert_eq!(stored[0].query_hash.len(), 32);
    assert!(stored.iter().all(|r| r.query_hash == stored[0].query_hash));

    let stats = catalog.interaction_stats().unwrap();
    assert_eq!(stats.events, 6);
    assert_eq!(stats.sessions, 1);
    assert_eq!(stats.oldest_ts, Some(now));
    assert_eq!(stats.newest_ts, Some(now));

    // An empty batch is not an error and writes nothing.
    assert_eq!(catalog.record_interactions(&[]).unwrap(), 0);
    assert_eq!(catalog.interaction_stats().unwrap().events, 6);
}

#[test]
fn retention_drops_events_past_the_age_limit_and_keeps_the_rest() {
    let (_dir, catalog) = catalog();
    let now = UnixNanos::now();
    let old = UnixNanos(now.0 - (100 * DAY).as_nanos() as i64);
    let recent = UnixNanos(now.0 - (2 * DAY).as_nanos() as i64);
    catalog
        .record_interactions(&[
            event(0, old, InteractionAction::Presented),
            event(1, recent, InteractionAction::Presented),
            event(2, now, InteractionAction::CopiedPath),
        ])
        .unwrap();

    let pruned = catalog
        .prune_interactions(InteractionRetention::default())
        .unwrap();
    assert_eq!(pruned, 1);
    let stored = rows(&catalog);
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|r| r.ts >= recent.0));

    // Pruning again is a no-op: the bound is a state, not a step.
    assert_eq!(
        catalog
            .prune_interactions(InteractionRetention::default())
            .unwrap(),
        0
    );
}

#[test]
fn the_row_cap_deletes_the_oldest_events_beyond_it() {
    let (_dir, catalog) = catalog();
    let now = UnixNanos::now();
    let batch: Vec<_> = (0..50)
        .map(|rank| event(rank, now, InteractionAction::Presented))
        .collect();
    catalog.record_interactions(&batch).unwrap();
    assert_eq!(catalog.interaction_stats().unwrap().events, 50);

    assert_eq!(
        InteractionRetention::default().max_rows,
        MAX_INTERACTION_ROWS
    );
    let cap = InteractionRetention {
        max_age: 365 * DAY,
        max_rows: 20,
    };
    assert_eq!(catalog.prune_interactions(cap).unwrap(), 30);
    let stored = rows(&catalog);
    assert_eq!(stored.len(), 20);
    // The survivors are the newest: the ranks kept are the last ones written.
    let ranks: Vec<_> = stored.iter().filter_map(|r| r.presented_rank).collect();
    assert_eq!(ranks, (30..50).collect::<Vec<u32>>());

    // Under the cap, nothing is deleted.
    assert_eq!(catalog.prune_interactions(cap).unwrap(), 0);
    assert_eq!(rows(&catalog).len(), 20);
}

#[test]
fn the_insert_path_bounds_the_table_without_an_operator() {
    let (_dir, catalog) = catalog();
    let now = UnixNanos::now();
    let stale = UnixNanos(now.0 - (120 * DAY).as_nanos() as i64);
    // Seed history that is already past the age limit, then keep recording:
    // the periodic prune inside the insert transaction must clear it without
    // anyone calling `prune_interactions`.
    let seed: Vec<_> = (0..200)
        .map(|rank| event(rank, stale, InteractionAction::Presented))
        .collect();
    catalog.record_interactions(&seed).unwrap();
    assert_eq!(catalog.interaction_stats().unwrap().events, 200);

    for round in 0..64 {
        catalog
            .record_interactions(&[event(round, now, InteractionAction::Exported)])
            .unwrap();
    }
    let stats = catalog.interaction_stats().unwrap();
    assert_eq!(stats.events, 64, "stale history was not pruned by inserts");
    assert!(stats.oldest_ts.unwrap() >= now);
}
