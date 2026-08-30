//! Source sync ledger (ADR-0015): stamping from every catalog write path,
//! bounded backfill, materialize-at-ship batches, consumer watermarks, and
//! tombstone collection. Platform neutral except the scan fixture, which
//! uses the default lister on a temporary tree.

use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::sync::{SyncBatch, SyncRow};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{ObjectId, ObjectKind, SourceId, SourceKind};
use std::collections::BTreeMap;
use std::sync::Arc;

const CONSUMER_A: [u8; 16] = [0xA1; 16];
const CONSUMER_B: [u8; 16] = [0xB2; 16];

struct Fx {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    catalog: Arc<Catalog>,
    source: SourceId,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::write(root.join("a/one.txt"), vec![b'1'; 100]).unwrap();
        std::fs::write(root.join("a/b/two.txt"), vec![b'2'; 200]).unwrap();
        std::fs::write(root.join("c/three.txt"), vec![b'3'; 300]).unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host = catalog.ensure_host("h", "windows").unwrap();
        let source = catalog
            .add_source(&NewSource {
                host_id: host,
                name: "fx".into(),
                kind: SourceKind::WindowsLocal,
                root_path: root.display().to_string(),
                aliases: vec![],
            })
            .unwrap();
        let fx = Fx {
            _dir: dir,
            root,
            catalog,
            source,
        };
        // Scan twice: NTFS updates a directory's own timestamps in its
        // parent's index lazily, so the first listing can carry stale
        // directory metadata that a second scan then legitimately changes.
        fx.scan();
        fx.scan();
        fx
    }

    fn scan(&self) {
        let lister = eidos_scanner::default_lister();
        run_scan(
            &self.catalog,
            self.source,
            lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
    }

    fn id(&self, rel: &str) -> ObjectId {
        self.catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .unwrap_or_else(|| panic!("{rel} not in catalog"))
    }

    fn key_of(&self, rel: &str) -> NativeKey {
        let o = self.catalog.get_object(self.id(rel)).unwrap().unwrap();
        NativeKey::from(o.native.expect("native identity"))
    }

    fn snapshot_of(&self, rel: &str, size: u64, modified_secs: i64) -> ObjectSnapshot {
        let o = self.catalog.get_object(self.id(rel)).unwrap().unwrap();
        ObjectSnapshot {
            native: o.native.expect("native identity"),
            kind: o.kind,
            attributes: o.attributes,
            size,
            allocated: size,
            link_count: 1,
            created: o.created,
            modified: Some(eidos_domain::UnixNanos(modified_secs * 1_000_000_000)),
            changed: Some(eidos_domain::UnixNanos(modified_secs * 1_000_000_000)),
            accessed: o.accessed,
            reparse_tag: 0,
        }
    }

    fn live_object_count(&self) -> u64 {
        let source = self.source;
        self.catalog
            .with_reader(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM objects WHERE source_id = ?1 AND deleted_at IS NULL",
                    [source.0],
                    |r| r.get::<_, i64>(0),
                )? as u64)
            })
            .unwrap()
    }

    /// Raw ledger counts `(live, tombstones)`, independent of cursors.
    fn ledger_counts(&self) -> (u64, u64) {
        let source = self.source;
        self.catalog
            .with_reader(|conn| {
                Ok(conn.query_row(
                    "SELECT SUM(deleted = 0), SUM(deleted = 1) FROM sync_rows WHERE source_id = ?1",
                    [source.0],
                    |r| {
                        Ok((
                            r.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64,
                            r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                        ))
                    },
                )?)
            })
            .unwrap()
    }

    fn enable_and_backfill(&self, batch: u32) -> u64 {
        self.catalog.sync_enable(self.source, None).unwrap();
        let mut steps = 0;
        loop {
            steps += 1;
            let p = self.catalog.sync_backfill(self.source, batch).unwrap();
            if p.done {
                break;
            }
            assert!(steps < 10_000, "backfill did not terminate");
        }
        steps
    }

    /// Every retained row, read from the compaction point (a cursor below
    /// it is rejected by design).
    fn all_rows(&self) -> SyncBatch {
        let from = self
            .catalog
            .sync_source(self.source)
            .unwrap()
            .unwrap()
            .compacted_through;
        self.catalog
            .sync_rows_after(self.source, from, u32::MAX)
            .unwrap()
    }

    fn seqs(&self) -> BTreeMap<ObjectId, (u64, bool)> {
        self.all_rows()
            .rows
            .iter()
            .map(|r| (r.object, (r.seq, r.image.is_none())))
            .collect()
    }
}

fn row_for(batch: &SyncBatch, object: ObjectId) -> &SyncRow {
    batch
        .rows
        .iter()
        .find(|r| r.object == object)
        .unwrap_or_else(|| panic!("no ledger row for {object}"))
}

#[test]
fn source_without_sync_stamps_nothing() {
    let fx = Fx::new();
    assert!(fx.catalog.sync_source(fx.source).unwrap().is_none());
    let parent = fx.key_of("a");
    let snap = fx.snapshot_of("a/one.txt", 5, 10);
    fx.catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent,
                name: "one.txt".into(),
                snapshot: snap,
            }],
            None,
        )
        .unwrap();
    fx.scan();
    assert!(fx.catalog.sync_source(fx.source).unwrap().is_none());
    assert!(fx.catalog.sync_sources().unwrap().is_empty());
    assert!(fx.catalog.sync_rows_after(fx.source, 0, 10).is_err());
}

#[test]
fn enable_backfills_live_objects_in_bounded_steps() {
    let fx = Fx::new();
    let live = fx.live_object_count();
    assert!(live >= 7, "fixture has root, a, a/b, c and three files");
    let steps = fx.enable_and_backfill(2);
    assert!(
        steps >= 4,
        "batch of 2 over {live} objects took {steps} steps"
    );
    let state = fx.catalog.sync_source(fx.source).unwrap().unwrap();
    assert!(state.ready);
    assert_eq!(state.head_seq, live);
    assert_eq!(state.compacted_through, 0);

    let batch = fx.all_rows();
    assert_eq!(batch.rows.len() as u64, live);
    assert_eq!(batch.through_seq, state.head_seq);
    assert_eq!(batch.epoch, state.epoch);
    let seqs: Vec<u64> = batch.rows.iter().map(|r| r.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "rows are seq-ordered");

    let two = row_for(&batch, fx.id("a/b/two.txt"));
    let image = two.image.as_ref().expect("live row has an image");
    assert_eq!(image.version, eidos_catalog::sync::SYNC_ROW_IMAGE_VERSION);
    assert_eq!(image.object.size, 200);
    assert_eq!(image.object.kind, ObjectKind::File);
    assert_eq!(image.entries.len(), 1);
    assert_eq!(image.entries[0].name, "two.txt");
    assert_eq!(image.entries[0].parent, Some(fx.id("a/b")));
    assert!(!image.entries[0].is_virtual);
    assert_eq!(image.archive_container, None);
    assert_eq!(two.generation, u64::from(image.object.generation));

    // Backfill is idempotent once ready.
    let again = fx.catalog.sync_backfill(fx.source, 2).unwrap();
    assert!(again.done);
    assert_eq!(again.stamped, 0);
}

#[test]
fn repeated_edits_to_one_object_ship_once() {
    let fx = Fx::new();
    fx.enable_and_backfill(100);
    let head0 = fx.catalog.sync_source(fx.source).unwrap().unwrap().head_seq;
    let before = fx.seqs();
    let one = fx.id("a/one.txt");
    for i in 0..500u64 {
        let snap = fx.snapshot_of("a/one.txt", 100 + i, 1_000 + i as i64);
        fx.catalog
            .apply_changes(fx.source, &[ChangeEvent::Update { snapshot: snap }], None)
            .unwrap();
    }
    let state = fx.catalog.sync_source(fx.source).unwrap().unwrap();
    assert!(state.head_seq >= head0 + 500);

    let after = fx.seqs();
    assert_eq!(after.len(), before.len(), "no row added or removed");
    assert!(after[&one].0 > before[&one].0);
    for (obj, (seq, _)) in &before {
        if *obj != one {
            assert_eq!(after[obj].0, *seq, "untouched {obj} was re-stamped");
        }
    }
    // Shipping from the pre-edit head yields exactly one row: the final image.
    let batch = fx.catalog.sync_rows_after(fx.source, head0, 1000).unwrap();
    assert_eq!(batch.rows.len(), 1);
    assert_eq!(batch.rows[0].object, one);
    let image = batch.rows[0].image.as_ref().unwrap();
    assert_eq!(image.object.size, 599);
    assert_eq!(batch.through_seq, state.head_seq);
}

#[test]
fn delete_ships_a_tombstone_collected_only_below_every_watermark() {
    let fx = Fx::new();
    fx.enable_and_backfill(100);
    let head0 = fx.catalog.sync_source(fx.source).unwrap().unwrap().head_seq;
    let two = fx.id("a/b/two.txt");
    let key = fx.key_of("a/b/two.txt");
    fx.catalog
        .apply_changes(fx.source, &[ChangeEvent::Delete { object: key }], None)
        .unwrap();
    let batch = fx.catalog.sync_rows_after(fx.source, head0, 100).unwrap();
    assert_eq!(batch.rows.len(), 1);
    assert_eq!(batch.rows[0].object, two);
    assert!(
        batch.rows[0].image.is_none(),
        "deletion ships as a tombstone"
    );
    let head1 = batch.head_seq;

    // Two consumers; only one has crossed the tombstone.
    assert!(fx
        .catalog
        .sync_acknowledge(fx.source, fx.all_rows().epoch, CONSUMER_A, head1)
        .unwrap());
    assert!(fx
        .catalog
        .sync_acknowledge(fx.source, fx.all_rows().epoch, CONSUMER_B, head0)
        .unwrap());
    let kept = fx.catalog.sync_collect(fx.source, 100).unwrap();
    assert_eq!(kept.removed_tombstones, 0);
    assert_eq!(kept.compacted_through, head0);
    assert_eq!(
        fx.ledger_counts().1,
        1,
        "tombstone retained above the floor"
    );
    assert!(fx.all_rows().rows.iter().any(|r| r.object == two));

    // Rewind is ignored; a beyond-head ack is an error.
    assert!(!fx
        .catalog
        .sync_acknowledge(fx.source, fx.all_rows().epoch, CONSUMER_A, head0)
        .unwrap());
    assert!(fx
        .catalog
        .sync_acknowledge(fx.source, fx.all_rows().epoch, CONSUMER_A, head1 + 1)
        .is_err());

    assert!(fx
        .catalog
        .sync_acknowledge(fx.source, fx.all_rows().epoch, CONSUMER_B, head1)
        .unwrap());
    let collected = fx.catalog.sync_collect(fx.source, 100).unwrap();
    assert_eq!(collected.removed_tombstones, 1);
    assert_eq!(collected.compacted_through, head1);
    assert_eq!(collected.remaining_below_floor, 0);
    // The tombstone is gone; live rows are never collected. Every retained
    // row is now at or below the compaction point, so a resuming consumer
    // sees nothing new and a fresh one must snapshot.
    assert_eq!(fx.ledger_counts(), (fx.live_object_count(), 0));
    assert!(fx.all_rows().rows.is_empty());

    // A cursor below the compaction point must repair, not resume.
    assert!(fx.catalog.sync_rows_after(fx.source, head0, 10).is_err());
    let consumers = fx.catalog.sync_consumers(fx.source).unwrap();
    assert_eq!(consumers.len(), 2);
    assert!(consumers.iter().all(|c| c.watermark == head1));
}

#[test]
fn rescan_stamps_only_changed_inserted_and_tombstoned_objects() {
    let fx = Fx::new();
    fx.enable_and_backfill(100);
    let before = fx.seqs();
    let one = fx.id("a/one.txt");
    let two = fx.id("a/b/two.txt");
    let three = fx.id("c/three.txt");
    let c = fx.id("c");

    std::fs::remove_file(fx.root.join("a/b/two.txt")).unwrap();
    std::fs::write(fx.root.join("a/one.txt"), vec![b'1'; 4_096]).unwrap();
    std::fs::write(fx.root.join("a/new.txt"), vec![b'n'; 7]).unwrap();
    fx.scan();

    let after = fx.seqs();
    assert!(after[&two].1, "not re-observed file is a tombstone");
    assert!(after[&two].0 > before[&two].0);
    assert!(after[&one].0 > before[&one].0, "modified file re-stamped");
    assert!(!after[&one].1);
    let new = fx.id("a/new.txt");
    assert!(after.contains_key(&new), "new file stamped on insert");
    assert!(!before.contains_key(&new));
    assert_eq!(
        after[&three], before[&three],
        "untouched file keeps its seq"
    );
    assert_eq!(after[&c], before[&c], "untouched directory keeps its seq");

    let batch = fx.all_rows();
    let one_row = row_for(&batch, one).image.as_ref().unwrap();
    assert_eq!(one_row.object.size, 4_096);
    assert_eq!(one_row.entries[0].name, "one.txt");
}

#[test]
fn reenable_mints_a_new_epoch_and_discards_history() {
    let fx = Fx::new();
    fx.enable_and_backfill(100);
    let first = fx.catalog.sync_source(fx.source).unwrap().unwrap();
    assert!(fx
        .catalog
        .sync_acknowledge(fx.source, first.epoch, CONSUMER_A, first.head_seq)
        .unwrap());

    fx.catalog.sync_enable(fx.source, Some(42)).unwrap();
    let second = fx.catalog.sync_source(fx.source).unwrap().unwrap();
    assert_ne!(first.epoch, second.epoch);
    assert_eq!(second.head_seq, 0);
    assert_eq!(second.journal_id, Some(42));
    assert!(!second.ready);
    assert!(fx.catalog.sync_consumers(fx.source).unwrap().is_empty());
    assert!(
        fx.catalog.sync_rows_after(fx.source, 0, 10).is_err(),
        "nothing ships before the backfill completes"
    );
    while !fx.catalog.sync_backfill(fx.source, 3).unwrap().done {}
    assert_eq!(fx.all_rows().rows.len() as u64, fx.live_object_count());

    assert!(fx.catalog.sync_disable(fx.source).unwrap());
    assert!(fx.catalog.sync_source(fx.source).unwrap().is_none());
    assert!(!fx.catalog.sync_disable(fx.source).unwrap());
    assert_eq!(format!("{}", second.epoch).len(), 36);
}

#[test]
fn limited_batches_cut_through_seq_and_chain() {
    let fx = Fx::new();
    fx.enable_and_backfill(100);
    let head = fx.catalog.sync_source(fx.source).unwrap().unwrap().head_seq;
    let mut cursor = 0;
    let mut seen = Vec::new();
    loop {
        let b = fx.catalog.sync_rows_after(fx.source, cursor, 3).unwrap();
        assert_eq!(b.after_seq, cursor);
        assert!(b
            .rows
            .iter()
            .all(|r| r.seq > cursor && r.seq <= b.through_seq));
        seen.extend(b.rows.iter().map(|r| r.object));
        if b.through_seq == head {
            assert!(b.rows.len() <= 3);
            break;
        }
        assert_eq!(b.rows.len(), 3, "a cut batch is full");
        assert_eq!(b.through_seq, b.rows.last().unwrap().seq);
        cursor = b.through_seq;
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len() as u64, fx.live_object_count());
}

// The readonly flag is cleared again so the temporary directory can be removed.
#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn attribute_only_change_is_stamped_on_rescan() {
    let fx = Fx::new();
    fx.enable_and_backfill(100);
    let before = fx.seqs();
    let three = fx.id("c/three.txt");
    let path = fx.root.join("c/three.txt");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    fx.scan();
    let after = fx.seqs();
    assert!(
        after[&three].0 > before[&three].0,
        "readonly flip re-stamped the row"
    );
    let image = row_for(&fx.all_rows(), three).image.clone().unwrap();
    assert!(
        image.object.attributes.0 & 0x1 != 0,
        "image carries the readonly attribute"
    );
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
}
