//! Real-SQLite adapters around the sync protocol (ADR-0023, sprint track B
//! gate): a source catalog's ledger drives a central catalog's replica
//! through the same admission, durability, fencing, compaction and repair
//! semantics the protocol simulator certifies, with no transport in between.
//! Each test plays the message exchange by hand so the catalog is the only
//! thing under test.

use eidos_catalog::changes::{ChangeEvent, ObjectSnapshot};
use eidos_catalog::replica::{BatchOutcome, HelloOutcome, RemoteNode, RemoteSourceDescriptor};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::sync::{record_digest, SyncRow};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{ContentState, ObjectId, SourceId, SourceKind, SourceState};
use eidos_sync::identity::SourceEpoch;
use eidos_sync::merkle::{leaf_index, MerkleTree};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

const CENTRAL: [u8; 16] = [0xC0; 16];
const NODE: [u8; 16] = [0x0A; 16];

struct Node {
    dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<Catalog>,
    source: SourceId,
}

impl Node {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::write(root.join("a/one.txt"), vec![b'1'; 100]).unwrap();
        std::fs::write(root.join("a/b/two.txt"), vec![b'2'; 200]).unwrap();
        std::fs::write(root.join("c/three.txt"), vec![b'3'; 300]).unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host = catalog.ensure_host("node-a", "windows").unwrap();
        let source = catalog
            .add_source(&NewSource {
                host_id: host,
                name: "docs".into(),
                kind: SourceKind::WindowsLocal,
                root_path: root.display().to_string(),
                aliases: vec![],
            })
            .unwrap();
        let node = Node {
            dir,
            root,
            catalog,
            source,
        };
        node.scan();
        node.scan();
        node.catalog.sync_enable(node.source, None).unwrap();
        while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
        node
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

    fn descriptor(&self) -> RemoteSourceDescriptor {
        RemoteSourceDescriptor {
            remote_source_id: self.source,
            name: "docs".into(),
            kind: SourceKind::WindowsLocal,
            root_path: self.root.display().to_string(),
            aliases: vec![],
        }
    }

    fn epoch(&self) -> SourceEpoch {
        self.catalog
            .sync_source(self.source)
            .unwrap()
            .unwrap()
            .epoch
            .to_source_epoch()
    }

    fn id(&self, rel: &str) -> ObjectId {
        self.catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .unwrap_or_else(|| panic!("{rel} not in catalog"))
    }

    /// Apply a metadata change to an object through the change-feed path,
    /// which stamps the ledger like a live watcher would.
    fn touch(&self, rel: &str, size: u64, modified_secs: i64) {
        let o = self.catalog.get_object(self.id(rel)).unwrap().unwrap();
        let native = o.native.expect("native identity");
        let snapshot = ObjectSnapshot {
            native,
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
        };
        self.catalog
            .apply_changes(self.source, &[ChangeEvent::Update { snapshot }], None)
            .unwrap();
    }

    /// Delete a file on disk and let a rescan tombstone it.
    fn delete(&self, rel: &str) {
        std::fs::remove_file(self.root.join(rel)).unwrap();
        self.scan();
    }

    /// Reopen the catalog from disk, the way a restarted process would.
    fn reopen(&mut self) {
        let path = self.dir.path().join("catalog.db");
        self.catalog = Catalog::open(path).unwrap();
    }

    /// Snapshot the catalog file so a later `restore` can rewind the source
    /// to an older state (a backup restore, clone, or VM revert).
    fn checkpoint(&self) -> Vec<u8> {
        self.catalog
            .with_writer(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                Ok(())
            })
            .unwrap();
        std::fs::read(self.dir.path().join("catalog.db")).unwrap()
    }

    fn restore(&mut self, image: &[u8]) {
        let path = self.dir.path().join("catalog.db");
        // Drop our handle first so the file can be replaced.
        self.catalog = Catalog::open(self.dir.path().join("scratch.db")).unwrap();
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(self.dir.path().join(format!("catalog.db{suffix}")));
        }
        std::fs::write(&path, image).unwrap();
        self.catalog = Catalog::open(path).unwrap();
    }

    fn image(&self) -> BTreeSet<(String, String, u64)> {
        image_of(&self.catalog, self.source)
    }

    fn head(&self) -> u64 {
        self.catalog
            .sync_source(self.source)
            .unwrap()
            .unwrap()
            .head_seq
    }

    fn watermark(&self) -> u64 {
        self.catalog
            .sync_consumers(self.source)
            .unwrap()
            .iter()
            .find(|c| c.consumer_id == CENTRAL)
            .map(|c| c.watermark)
            .unwrap_or(0)
    }
}

/// Every live entry's rendered path, kind and size.
fn image_of(catalog: &Catalog, source: SourceId) -> BTreeSet<(String, String, u64)> {
    let mut out = BTreeSet::new();
    catalog
        .for_each_projection_row(source, |row| {
            out.insert((row.path.clone(), row.kind.as_str().to_string(), row.size));
            Ok(())
        })
        .unwrap();
    out
}

struct Central {
    dir: tempfile::TempDir,
    catalog: Arc<Catalog>,
}

impl Central {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        catalog.ensure_host("central", "windows").unwrap();
        Central { dir, catalog }
    }

    fn reopen(&mut self) {
        self.catalog = Catalog::open(self.dir.path().join("catalog.db")).unwrap();
    }

    fn node() -> RemoteNode {
        RemoteNode {
            node_id: NODE,
            name: "node-a".into(),
            platform: "windows".into(),
        }
    }

    fn source_for(&self, node: &Node) -> SourceId {
        self.catalog
            .replica_ensure_source(&Self::node(), &node.descriptor(), node.epoch())
            .unwrap()
            .source_id
    }

    fn image(&self, source: SourceId) -> BTreeSet<(String, String, u64)> {
        image_of(&self.catalog, source)
    }

    fn applied_seq(&self, source: SourceId) -> u64 {
        self.catalog
            .replica_source(source)
            .unwrap()
            .unwrap()
            .admission
            .applied_seq
    }
}

/// Play the protocol between a node and the central until the central's
/// cursor reaches the node's head, acknowledging every applied batch.
/// Returns the batches applied.
fn converge(node: &Node, central: &Central, limit: u32) -> u64 {
    let source = central.source_for(node);
    let mut applied = 0;
    for _round in 0..64 {
        let state = node.catalog.sync_source(node.source).unwrap().unwrap();
        let outcome = central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap();
        let (after, epoch) = match outcome {
            HelloOutcome::Resume {
                after_seq,
                requires_repair: false,
                epoch,
            } => (after_seq, epoch),
            HelloOutcome::FullResync { epoch } => (0, epoch),
            other => panic!("unexpected hello outcome {other:?}"),
        };
        if after >= state.head_seq {
            return applied;
        }
        let batch = node
            .catalog
            .sync_rows_after(node.source, after, limit)
            .unwrap();
        assert_eq!(batch.epoch.to_source_epoch(), epoch);
        match central.catalog.replica_apply_batch(source, &batch).unwrap() {
            BatchOutcome::Applied { through_seq, .. } => {
                node.catalog
                    .sync_acknowledge(node.source, CENTRAL, through_seq)
                    .unwrap();
                applied += 1;
            }
            other => panic!("unexpected batch outcome {other:?}"),
        }
    }
    panic!("did not converge");
}

fn ship_batch(
    node: &Node,
    central: &Central,
    source: SourceId,
    after: u64,
    limit: u32,
) -> BatchOutcome {
    let batch = node
        .catalog
        .sync_rows_after(node.source, after, limit)
        .unwrap();
    central.catalog.replica_apply_batch(source, &batch).unwrap()
}

#[test]
fn a_bounded_stream_reproduces_the_source_image_with_children_before_parents() {
    let node = Node::new();
    let central = Central::new();
    // Touch a directory after its children so its sequence is later than
    // theirs: a one-row batch then delivers a child before its parent.
    node.touch("a", 0, 1_700_000_000);
    node.touch("a/b", 0, 1_700_000_001);
    let batches = converge(&node, &central, 1);
    let source = central.source_for(&node);
    assert!(batches >= 7, "one row per batch: {batches}");
    assert_eq!(central.image(source), node.image());
    let s = central.catalog.get_source(source).unwrap().unwrap();
    assert_eq!(s.kind, SourceKind::Remote);
    assert_eq!(s.state, SourceState::MetadataComplete);
    assert!(s.root_object_id.is_some());
    // Content is not replicated and says so.
    let two = central
        .catalog
        .resolve_relative(source, "a/b/two.txt")
        .unwrap()
        .unwrap();
    let obj = central.catalog.get_object(two).unwrap().unwrap();
    assert_eq!(obj.content_state, ContentState::NotReplicated);
    assert_eq!(obj.size, 200);
    // Aggregates can be rebuilt from the applied rows.
    central.catalog.replica_rebuild_aggregates(source).unwrap();
    let root = central
        .catalog
        .get_source(source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let agg = central.catalog.directory_aggregate(root).unwrap().unwrap();
    assert_eq!(agg.file_count, 3);
}

#[test]
fn duplicate_and_overlapping_batches_are_idempotent() {
    let node = Node::new();
    let central = Central::new();
    let source = central.source_for(&node);
    converge(&node, &central, 2);
    let before = central.image(source);
    // The same batch again: nothing changes, the acknowledgement repeats.
    match ship_batch(&node, &central, source, 0, 2) {
        BatchOutcome::AlreadyApplied { applied_seq } => assert_eq!(applied_seq, node.head()),
        other => panic!("{other:?}"),
    }
    // New work, then an overlapping batch that starts before the cursor.
    node.touch("a/one.txt", 101, 1_700_000_000);
    let head = node.head();
    match ship_batch(&node, &central, source, 0, u32::MAX) {
        BatchOutcome::Stale { applied_seq } => assert!(applied_seq < head),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        central.image(source),
        before,
        "a stale batch changed nothing"
    );
    converge(&node, &central, 2);
    assert_eq!(central.image(source), node.image());
    assert_eq!(central.applied_seq(source), head);
}

#[test]
fn a_central_that_stops_before_acknowledging_is_caught_up_by_the_resend() {
    let node = Node::new();
    let mut central = Central::new();
    let source = central.source_for(&node);
    let batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let outcome = central.catalog.replica_apply_batch(source, &batch).unwrap();
    assert!(matches!(outcome, BatchOutcome::Applied { .. }));
    // The process dies before the ACK leaves: the node never learns.
    central.reopen();
    assert_eq!(node.watermark(), 0);
    assert_eq!(
        central.applied_seq(source),
        batch.through_seq,
        "the apply was durable"
    );
    // The node resends from its watermark; the replica answers with its
    // durable cursor and the node catches up without re-applying.
    match ship_batch(&node, &central, source, 0, u32::MAX) {
        BatchOutcome::AlreadyApplied { applied_seq } => {
            node.catalog
                .sync_acknowledge(node.source, CENTRAL, applied_seq)
                .unwrap();
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(node.watermark(), node.head());
    assert_eq!(central.image(source), node.image());
}

#[test]
fn a_node_that_restarts_with_unacknowledged_work_resumes_from_the_same_cursor() {
    let mut node = Node::new();
    let central = Central::new();
    converge(&node, &central, 4);
    let source = central.source_for(&node);
    node.touch("c/three.txt", 301, 1_700_000_000);
    node.touch("a/one.txt", 102, 1_700_000_001);
    let watermark = node.watermark();
    // Restart: RAM state is gone, the ledger and watermark are not.
    node.reopen();
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    let outcome = central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            state.compacted_through,
        )
        .unwrap();
    assert_eq!(
        outcome,
        HelloOutcome::Resume {
            epoch: state.epoch.to_source_epoch(),
            after_seq: watermark,
            requires_repair: false
        }
    );
    converge(&node, &central, 4);
    assert_eq!(central.image(source), node.image());
}

#[test]
fn a_same_epoch_rewind_is_fenced_and_a_rewritten_history_is_a_fork() {
    let mut node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let image = node.checkpoint();
    node.touch("a/one.txt", 111, 1_700_000_000);
    node.touch("a/one.txt", 112, 1_700_000_001);
    converge(&node, &central, 8);
    let applied = central.applied_seq(source);
    // Restore the node's catalog from the backup: its head is now below
    // the central's cursor within the same epoch.
    node.restore(&image);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(state.head_seq < applied);
    let outcome = central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            state.compacted_through,
        )
        .unwrap();
    match outcome {
        HelloOutcome::Rejected { reason } => assert!(reason.contains("rewind"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // The restored node keeps working and overtakes the cursor with a
    // different history: same cursor, different chain, fenced as a fork.
    node.touch("c/three.txt", 333, 1_700_000_002);
    node.touch("c/three.txt", 334, 1_700_000_003);
    node.touch("c/three.txt", 335, 1_700_000_004);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(state.head_seq >= applied);
    let outcome = central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            state.compacted_through,
        )
        .unwrap();
    match outcome {
        HelloOutcome::Resume { after_seq, .. } => {
            assert_eq!(after_seq, applied);
            match ship_batch(&node, &central, source, after_seq, u32::MAX) {
                BatchOutcome::Rejected { reason } => {
                    assert!(reason.contains("history fork"), "{reason}")
                }
                other => panic!("{other:?}"),
            }
        }
        HelloOutcome::Rejected { reason } => assert!(reason.contains("fork"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // Nothing from the forked history reached the replica.
    assert_eq!(central.applied_seq(source), applied);
    let three = central
        .catalog
        .resolve_relative(source, "c/three.txt")
        .unwrap()
        .unwrap();
    assert_eq!(
        central.catalog.get_object(three).unwrap().unwrap().size,
        300
    );
}

#[test]
fn an_epoch_change_streams_a_full_resync_and_retires_rows_the_new_epoch_lacks() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let first_epoch = node.epoch();
    // A rebuild: the source is re-enabled with a fresh epoch after a file
    // vanished while sync was off, so no tombstone for it ever ships.
    node.catalog.sync_disable(node.source).unwrap();
    std::fs::remove_file(node.root.join("c/three.txt")).unwrap();
    node.scan();
    node.catalog.sync_enable(node.source, None).unwrap();
    while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
    assert_ne!(node.epoch(), first_epoch);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    let outcome = central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            state.compacted_through,
        )
        .unwrap();
    assert_eq!(
        outcome,
        HelloOutcome::FullResync {
            epoch: state.epoch.to_source_epoch()
        }
    );
    // Stream the new epoch in small pieces: until the stream passes the
    // offered head the old row is still there (search stays available),
    // afterwards it is gone.
    let mut after = 0;
    let mut saw_retirement = false;
    loop {
        let batch = node.catalog.sync_rows_after(node.source, after, 2).unwrap();
        match central.catalog.replica_apply_batch(source, &batch).unwrap() {
            BatchOutcome::Applied {
                through_seq,
                retired_rows,
                ..
            } => {
                node.catalog
                    .sync_acknowledge(node.source, CENTRAL, through_seq)
                    .unwrap();
                if retired_rows > 0 {
                    saw_retirement = true;
                    assert_eq!(retired_rows, 1, "only the vanished file is retired");
                }
                after = through_seq;
                if through_seq >= state.head_seq {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(saw_retirement);
    assert_eq!(central.image(source), node.image());
    assert!(central
        .catalog
        .resolve_relative(source, "c/three.txt")
        .unwrap()
        .is_none());
    // The retired epoch can never be admitted again.
    let outcome = central
        .catalog
        .replica_admit_hello(source, first_epoch, 1, [0u8; 32], 0)
        .unwrap();
    assert!(matches!(outcome, HelloOutcome::Rejected { .. }));
}

#[test]
fn a_cursor_below_the_compaction_floor_is_repaired_by_merkle_leaves() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    // The node loses its record of the central (a consumer table restored
    // from before enrollment) while a file is deleted and another changes:
    // with no consumer waiting, the tombstone is collected and the floor
    // moves past the central's cursor.
    node.catalog
        .with_writer(|conn| {
            conn.execute("DELETE FROM sync_consumers", [])?;
            Ok(())
        })
        .unwrap();
    node.delete("a/b/two.txt");
    node.touch("a/one.txt", 150, 1_700_000_000);
    let stats = node.catalog.sync_collect(node.source, 100).unwrap();
    assert_eq!(stats.removed_tombstones, 1);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(state.compacted_through > central.applied_seq(source));
    let outcome = central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            state.compacted_through,
        )
        .unwrap();
    assert!(matches!(
        outcome,
        HelloOutcome::Resume {
            requires_repair: true,
            ..
        }
    ));
    assert!(node
        .catalog
        .sync_rows_after(node.source, central.applied_seq(source), 8)
        .is_err());
    // Repair: the node offers its leaf manifest, the central asks for the
    // leaves that differ, the node answers with those rows.
    let (state, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    let leaf_bits = 4;
    let tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries
            .iter()
            .map(|e| record_digest(e.object, e.generation, e.deleted)),
    );
    let request = central
        .catalog
        .replica_repair_offer(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &tree.leaf_hashes(),
        )
        .unwrap();
    let leaves = match request {
        eidos_catalog::replica::RepairOfferOutcome::Request { leaves, .. } => leaves,
        other => panic!("{other:?}"),
    };
    assert!(!leaves.is_empty());
    let wanted: BTreeSet<u32> = leaves.iter().copied().collect();
    let objects: Vec<ObjectId> = entries
        .iter()
        .filter(|e| wanted.contains(&leaf_index(leaf_bits, e.object)))
        .map(|e| e.object)
        .collect();
    let (state2, rows): (_, Vec<SyncRow>) = node
        .catalog
        .sync_rows_for_objects(node.source, &objects)
        .unwrap();
    assert_eq!(state2.head_seq, state.head_seq);
    let outcome = central
        .catalog
        .replica_apply_repair(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &leaves,
            &rows,
        )
        .unwrap();
    match outcome {
        eidos_catalog::replica::RepairOutcome::Applied { removed, .. } => {
            assert_eq!(
                removed, 1,
                "the collected tombstone is an authoritative absence"
            );
        }
        other => panic!("{other:?}"),
    }
    node.catalog
        .sync_acknowledge(node.source, CENTRAL, state.head_seq)
        .unwrap();
    assert_eq!(central.applied_seq(source), state.head_seq);
    assert_eq!(central.image(source), node.image());
    assert!(central
        .catalog
        .resolve_relative(source, "a/b/two.txt")
        .unwrap()
        .is_none());
    // Both Merkle images now agree leaf for leaf.
    let central_tree =
        MerkleTree::with_leaf_bits(leaf_bits, central.catalog.replica_digests(source).unwrap());
    assert_eq!(central_tree.leaf_hashes(), tree.leaf_hashes());
}

#[test]
fn two_nodes_with_the_same_source_name_and_path_stay_distinct() {
    let a = Node::new();
    let b = Node::new();
    let central = Central::new();
    converge(&a, &central, 8);
    let node_b = RemoteNode {
        node_id: [0x0B; 16],
        name: "node-b".into(),
        platform: "windows".into(),
    };
    let mut descriptor = b.descriptor();
    descriptor.root_path = a.root.display().to_string();
    let source_b = central
        .catalog
        .replica_ensure_source(&node_b, &descriptor, b.epoch())
        .unwrap()
        .source_id;
    let source_a = central.source_for(&a);
    assert_ne!(source_a, source_b);
    let names: BTreeSet<String> = central
        .catalog
        .list_sources()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.contains("node-a/docs"));
    assert!(names.contains("node-b/docs"));
    let hosts: BTreeSet<_> = central
        .catalog
        .list_sources()
        .unwrap()
        .into_iter()
        .map(|s| s.host_id)
        .collect();
    assert_eq!(hosts.len(), 2);
}
