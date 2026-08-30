//! Real-SQLite adapters around the sync protocol (ADR-0023, sprint track B
//! gate): a source catalog's ledger drives a central catalog's replica
//! through the same admission, durability, fencing, compaction and repair
//! semantics the protocol simulator certifies, with no transport in between.
//! Each test plays the message exchange by hand so the catalog is the only
//! thing under test.

use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
use eidos_catalog::fleet::{FleetPeer, NodeId, PeerRole};
use eidos_catalog::replica::{
    BatchOutcome, HelloOutcome, RemoteNode, RemoteSourceDescriptor, RepairOutcome,
};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::sync::{record_digest, SyncBatch, SyncEpoch, SyncRow, CHAIN_GENESIS};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{
    ContentState, FileAttributes, ObjectId, SourceId, SourceKind, SourceState, UnixNanos,
};
use eidos_sync::identity::SourceEpoch;
use eidos_sync::merkle::{leaf_index, MerkleTree, MIN_FLEET_LEAF_BITS};
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
            case_sensitive: false,
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

    fn snapshot(&self, rel: &str) -> ObjectSnapshot {
        let o = self.catalog.get_object(self.id(rel)).unwrap().unwrap();
        ObjectSnapshot {
            native: o.native.expect("native identity"),
            kind: o.kind,
            attributes: o.attributes,
            size: o.size,
            allocated: o.allocated,
            link_count: o.link_count,
            created: o.created,
            modified: o.modified,
            changed: o.changed,
            accessed: o.accessed,
            reparse_tag: o.reparse_tag,
        }
    }

    fn set_attributes(&self, rel: &str, attributes: FileAttributes) {
        let mut snapshot = self.snapshot(rel);
        snapshot.attributes = attributes;
        self.catalog
            .apply_changes(self.source, &[ChangeEvent::Update { snapshot }], None)
            .unwrap();
    }

    fn rename_one(&self, new_name: &str) {
        let parent = NativeKey::from(
            self.catalog
                .get_object(self.id("a"))
                .unwrap()
                .unwrap()
                .native
                .expect("native identity"),
        );
        let snapshot = self.snapshot("a/one.txt");
        self.catalog
            .apply_changes(
                self.source,
                &[
                    ChangeEvent::Unlink {
                        parent,
                        name: "one.txt".into(),
                    },
                    ChangeEvent::Link {
                        parent,
                        name: new_name.into(),
                        snapshot,
                    },
                ],
                None,
            )
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
                    .sync_acknowledge(node.source, state.epoch, CENTRAL, through_seq)
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
    assert!(central
        .catalog
        .set_source_kind(source, SourceKind::WindowsLocal)
        .is_err());
    let completeness = central.catalog.source_completeness(source).unwrap();
    assert!(completeness.metadata_complete);
    assert!(!completeness.content_complete);
    assert!(completeness.content_not_replicated);
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
fn a_batch_cannot_publish_below_the_durable_reported_head() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let applied = central.applied_seq(source);
    node.touch("a/one.txt", 101, 1_700_000_000);
    node.touch("a/b/two.txt", 201, 1_700_000_001);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(state.head_seq > applied + 1);
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap(),
        HelloOutcome::Resume { .. }
    ));
    assert!(
        !central
            .catalog
            .source_completeness(source)
            .unwrap()
            .metadata_complete
    );

    let mut regressing = node
        .catalog
        .sync_rows_after(node.source, applied, 1)
        .unwrap();
    assert!(regressing.through_seq < state.head_seq);
    regressing.head_seq = regressing.through_seq;
    match central
        .catalog
        .replica_apply_batch(source, &regressing)
        .unwrap()
    {
        BatchOutcome::Rejected { reason } => assert!(reason.contains("regresses"), "{reason}"),
        other => panic!("regressing batch was accepted: {other:?}"),
    }
    assert_eq!(central.applied_seq(source), applied);
    assert!(
        !central
            .catalog
            .source_completeness(source)
            .unwrap()
            .metadata_complete
    );

    let legitimate = node
        .catalog
        .sync_rows_after(node.source, applied, 1)
        .unwrap();
    let partial_head = legitimate.through_seq;
    let partial_chain = legitimate.through_chain;
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &legitimate)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    match central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            partial_head,
            partial_chain,
            0,
        )
        .unwrap()
    {
        HelloOutcome::Rejected { reason } => assert!(reason.contains("rewind"), "{reason}"),
        other => panic!("active-epoch head regression was accepted: {other:?}"),
    }
    assert_eq!(
        central
            .catalog
            .replica_source(source)
            .unwrap()
            .unwrap()
            .reported_head,
        state.head_seq
    );
}

#[test]
fn one_batch_can_swap_two_sibling_names() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    std::fs::write(node.root.join("a/four.txt"), vec![b'4'; 400]).unwrap();
    node.scan();
    converge(&node, &central, 8);

    std::fs::rename(node.root.join("a/one.txt"), node.root.join("a/swap.tmp")).unwrap();
    std::fs::rename(node.root.join("a/four.txt"), node.root.join("a/one.txt")).unwrap();
    std::fs::rename(node.root.join("a/swap.tmp"), node.root.join("a/four.txt")).unwrap();
    node.scan();
    converge(&node, &central, 1);

    assert_eq!(central.image(source), node.image());
    let one = central
        .catalog
        .resolve_relative(source, "a/one.txt")
        .unwrap()
        .unwrap();
    let four = central
        .catalog
        .resolve_relative(source, "a/four.txt")
        .unwrap()
        .unwrap();
    assert_eq!(central.catalog.get_object(one).unwrap().unwrap().size, 400);
    assert_eq!(central.catalog.get_object(four).unwrap().unwrap().size, 100);
}

#[test]
fn a_batch_is_bound_to_the_remote_source_it_names() {
    let node = Node::new();
    let central = Central::new();
    let source = central.source_for(&node);
    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    batch.source_id = SourceId(node.source.0 + 1_000);

    match central.catalog.replica_apply_batch(source, &batch).unwrap() {
        BatchOutcome::Rejected { reason } => assert!(reason.contains("does not match"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(central.applied_seq(source), 0);
    assert!(central.image(source).is_empty());

    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let row = batch
        .rows
        .iter_mut()
        .find(|row| row.image.is_some())
        .expect("a live row");
    row.image.as_mut().unwrap().object.id = ObjectId(row.object.0 + 10_000);
    match central.catalog.replica_apply_batch(source, &batch).unwrap() {
        BatchOutcome::Rejected { reason } => assert!(reason.contains("identity"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(central.applied_seq(source), 0);
    assert!(central.image(source).is_empty());

    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let image = batch
        .rows
        .iter_mut()
        .find_map(|row| row.image.as_mut())
        .expect("a live row");
    image.object.source_id = SourceId(node.source.0 + 10_000);
    match central.catalog.replica_apply_batch(source, &batch).unwrap() {
        BatchOutcome::Rejected { reason } => assert!(reason.contains("identity"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(central.applied_seq(source), 0);
    assert!(central.image(source).is_empty());
}

#[test]
fn remote_sequences_that_do_not_fit_sqlite_are_rejected_before_casting() {
    let node = Node::new();
    let central = Central::new();
    let source = central.source_for(&node);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                i64::MAX as u64 + 1,
                state.head_chain,
                0,
            )
            .unwrap(),
        HelloOutcome::Rejected { .. }
    ));
    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    batch.through_seq = u64::MAX;
    batch.head_seq = u64::MAX;
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &batch).unwrap(),
        BatchOutcome::Rejected { .. }
    ));

    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let row = batch
        .rows
        .iter_mut()
        .find(|row| row.image.is_some())
        .unwrap();
    row.generation = u64::from(u32::MAX) + 1;
    row.image = None;
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &batch).unwrap(),
        BatchOutcome::Rejected { .. }
    ));

    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let row = batch
        .rows
        .iter_mut()
        .find(|row| row.image.is_some())
        .unwrap();
    row.generation += 1;
    assert_ne!(
        row.generation,
        u64::from(row.image.as_ref().unwrap().object.generation)
    );
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &batch).unwrap(),
        BatchOutcome::Rejected { .. }
    ));

    let mut batch = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let image = batch
        .rows
        .iter_mut()
        .find_map(|row| row.image.as_mut())
        .unwrap();
    let entry = image.entries[0].clone();
    image.entries = vec![entry; 100_001];
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &batch).unwrap(),
        BatchOutcome::Rejected { .. }
    ));
    assert_eq!(central.applied_seq(source), 0);
}

#[test]
fn a_replicated_source_cannot_be_enabled_as_a_second_hop_shipper() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    assert!(central.catalog.sync_enable(source, None).is_err());
    assert!(central.catalog.sync_source(source).unwrap().is_none());
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
                .sync_acknowledge(node.source, batch.epoch, CENTRAL, applied_seq)
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
fn an_ack_from_a_retired_epoch_cannot_advance_the_new_epoch() {
    let node = Node::new();
    let first = node.catalog.sync_source(node.source).unwrap().unwrap();
    node.catalog.sync_enable(node.source, None).unwrap();
    while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
    let second = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert_ne!(first.epoch, second.epoch);

    assert!(node
        .catalog
        .sync_acknowledge(node.source, first.epoch, CENTRAL, first.head_seq)
        .is_err());
    assert!(node.catalog.sync_consumers(node.source).unwrap().is_empty());
    assert!(node
        .catalog
        .sync_acknowledge(node.source, second.epoch, CENTRAL, second.head_seq)
        .unwrap());
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
    assert_eq!(
        central
            .catalog
            .replica_source(source)
            .unwrap()
            .unwrap()
            .reported_head,
        applied,
        "a rejected rewind cannot replace the last accepted coverage report"
    );
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
fn a_same_height_rename_only_rewrite_is_fenced_as_a_history_fork() {
    let mut node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let checkpoint = node.checkpoint();
    let generation = node
        .catalog
        .get_object(node.id("a/one.txt"))
        .unwrap()
        .unwrap()
        .generation;

    node.rename_one("alpha.txt");
    assert_eq!(
        node.catalog
            .get_object(node.id("a/alpha.txt"))
            .unwrap()
            .unwrap()
            .generation,
        generation,
        "rename-only updates do not bump object generation"
    );
    converge(&node, &central, 8);
    let accepted_head = central.applied_seq(source);

    node.restore(&checkpoint);
    node.rename_one("beta.txt");
    let fork = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert_eq!(
        fork.head_seq, accepted_head,
        "both branches minted two touches"
    );
    assert_eq!(
        node.catalog
            .get_object(node.id("a/beta.txt"))
            .unwrap()
            .unwrap()
            .generation,
        generation
    );
    match central
        .catalog
        .replica_admit_hello(
            source,
            fork.epoch.to_source_epoch(),
            fork.head_seq,
            fork.head_chain,
            fork.compacted_through,
        )
        .unwrap()
    {
        HelloOutcome::Rejected { reason } => assert!(reason.contains("history fork"), "{reason}"),
        other => panic!("rename-only fork was admitted: {other:?}"),
    }
    assert!(central
        .catalog
        .resolve_relative(source, "a/alpha.txt")
        .unwrap()
        .is_some());
    assert!(central
        .catalog
        .resolve_relative(source, "a/beta.txt")
        .unwrap()
        .is_none());
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
    // The source keeps changing after the hello. Its latest touch now falls
    // beyond the resync target, so crossing the old target is not yet a
    // complete snapshot and must not retire the previous image.
    node.touch("a/one.txt", 123, 1_700_000_100);
    let streaming = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(streaming.head_seq > state.head_seq);
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
                    .sync_acknowledge(node.source, state.epoch, CENTRAL, through_seq)
                    .unwrap();
                if retired_rows > 0 {
                    saw_retirement = true;
                    assert_eq!(retired_rows, 1, "only the vanished file is retired");
                }
                if through_seq == state.head_seq {
                    assert_eq!(retired_rows, 0, "the batch has not reached its own head");
                    let one = central
                        .catalog
                        .resolve_relative(source, "a/one.txt")
                        .unwrap()
                        .unwrap();
                    assert_eq!(central.catalog.get_object(one).unwrap().unwrap().size, 100);
                }
                after = through_seq;
                if through_seq >= streaming.head_seq {
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
    let (_, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    let source_tree = MerkleTree::with_leaf_bits(
        4,
        entries
            .iter()
            .map(|e| record_digest(e.object, e.generation, e.deleted, &e.image_hash)),
    );
    let replica_tree =
        MerkleTree::with_leaf_bits(4, central.catalog.replica_digests(source).unwrap());
    assert_eq!(replica_tree.leaf_hashes(), source_tree.leaf_hashes());
    // The retired epoch can never be admitted again.
    let outcome = central
        .catalog
        .replica_admit_hello(source, first_epoch, 1, [0u8; 32], 0)
        .unwrap();
    assert!(matches!(outcome, HelloOutcome::Rejected { .. }));
}

#[test]
fn an_empty_new_epoch_is_a_complete_snapshot_that_retires_the_old_image() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let epoch = SourceEpoch::random_v4(44, 55);
    assert_eq!(
        central
            .catalog
            .replica_admit_hello(source, epoch, 0, CHAIN_GENESIS, 0)
            .unwrap(),
        HelloOutcome::FullResync { epoch }
    );
    let snapshot = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch),
        after_seq: 0,
        after_chain: CHAIN_GENESIS,
        through_seq: 0,
        through_chain: CHAIN_GENESIS,
        head_seq: 0,
        rows: Vec::new(),
    };
    match central
        .catalog
        .replica_apply_batch(source, &snapshot)
        .unwrap()
    {
        BatchOutcome::Applied {
            through_seq,
            retired_rows,
            ..
        } => {
            assert_eq!(through_seq, 0);
            assert!(retired_rows > 0);
        }
        other => panic!("{other:?}"),
    }
    let state = central.catalog.replica_source(source).unwrap().unwrap();
    assert_eq!(state.admission.epoch, epoch);
    assert_eq!(state.admission.applied_seq, 0);
    assert_eq!(state.resync_target, None);
    assert!(central.image(source).is_empty());
    assert!(central.catalog.replica_digests(source).unwrap().is_empty());
    assert_eq!(
        central
            .catalog
            .get_source(source)
            .unwrap()
            .unwrap()
            .root_object_id,
        None
    );
}

#[test]
fn retirement_targets_are_epoch_local_and_completeness_tracks_newer_heads() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let old_epoch = central
        .catalog
        .replica_source(source)
        .unwrap()
        .unwrap()
        .admission
        .epoch;
    central
        .catalog
        .with_writer(|conn| {
            let tx = conn.transaction()?;
            for remote in 1_000_000..1_010_001 {
                tx.execute(
                    "INSERT INTO objects
                        (source_id, kind, identity_confidence, first_seen_generation,
                         last_seen_generation)
                     VALUES (?1, 'file', 'path', 1, 1)",
                    [source.0],
                )?;
                let local = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO sync_replica_rows
                        (source_id, remote_object_id, local_object_id, epoch, seq,
                         generation, deleted, image_hash, placeholder)
                     VALUES (?1, ?2, ?3, ?4, 1, 1, 0, zeroblob(32), 0)",
                    rusqlite::params![source.0, remote, local, old_epoch.as_bytes().as_slice()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();

    let before_rebuild = central.catalog.replica_source(source).unwrap().unwrap();
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let mismatched_hashes = vec![[0xFF; 32]; 1usize << leaf_bits];
    let request = central
        .catalog
        .replica_repair_offer(
            source,
            old_epoch,
            before_rebuild.admission.applied_seq,
            before_rebuild.admission.applied_chain,
            leaf_bits,
            &mismatched_hashes,
        )
        .unwrap();
    let leaves = match request {
        eidos_catalog::replica::RepairOfferOutcome::Request { leaves, .. } => leaves,
        other => panic!("{other:?}"),
    };
    match central
        .catalog
        .replica_apply_repair(
            source,
            old_epoch,
            before_rebuild.admission.applied_seq,
            before_rebuild.admission.applied_chain,
            leaf_bits,
            &leaves,
            &[],
        )
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => {
            assert!(reason.contains("transaction limit"), "{reason}")
        }
        other => panic!("oversized repair transaction was accepted: {other:?}"),
    }

    let epoch = SourceEpoch::random_v4(303, 404);
    let epoch_chain = [0x33; 32];
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(source, epoch, 100, epoch_chain, 0)
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    let empty = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch),
        after_seq: 0,
        after_chain: CHAIN_GENESIS,
        through_seq: 100,
        through_chain: epoch_chain,
        head_seq: 100,
        rows: vec![],
    };
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &empty).unwrap(),
        BatchOutcome::Applied { .. }
    ));
    assert_eq!(
        central
            .catalog
            .replica_source(source)
            .unwrap()
            .unwrap()
            .resync_target,
        Some(100)
    );

    let next_epoch = SourceEpoch::random_v4(505, 606);
    let next_chain = [0x55; 32];
    assert_eq!(
        central
            .catalog
            .replica_admit_hello(source, next_epoch, 10, next_chain, 0)
            .unwrap(),
        HelloOutcome::FullResync { epoch: next_epoch }
    );
    assert_eq!(
        central
            .catalog
            .replica_source(source)
            .unwrap()
            .unwrap()
            .resync_target,
        Some(10),
        "a new epoch must not inherit the previous epoch's higher target"
    );
    let next_empty = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(next_epoch),
        after_seq: 0,
        after_chain: CHAIN_GENESIS,
        through_seq: 10,
        through_chain: next_chain,
        head_seq: 10,
        rows: vec![],
    };
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &next_empty)
            .unwrap(),
        BatchOutcome::Applied { retired_rows, .. } if retired_rows > 0
    ));

    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(source, next_epoch, 11, [0x66; 32], 0)
            .unwrap(),
        HelloOutcome::Resume { after_seq: 10, .. }
    ));
    let state = central.catalog.replica_source(source).unwrap().unwrap();
    assert_eq!(state.resync_target, None);
    assert_eq!(state.admission.applied_seq, 10);
    assert_eq!(state.reported_head, 11);
    assert!(
        !central
            .catalog
            .source_completeness(source)
            .unwrap()
            .metadata_complete
    );
}

#[test]
fn a_full_resync_replaces_the_root_mapping_when_remote_object_ids_change() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let old_root = central
        .catalog
        .get_source(source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let mut root = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap()
        .rows
        .into_iter()
        .find(|row| {
            row.image
                .as_ref()
                .is_some_and(|image| image.entries.iter().any(|entry| entry.parent.is_none()))
        })
        .expect("source root row");
    let remote_root = ObjectId(root.object.0 + 1_000_000);
    root.object = remote_root;
    root.seq = 2;
    root.image.as_mut().unwrap().object.id = remote_root;
    let (_, mut child_rows) = node
        .catalog
        .sync_rows_for_objects(node.source, &[node.id("a")])
        .unwrap();
    let mut child = child_rows.pop().expect("child row");
    child.seq = 1;
    for entry in &mut child.image.as_mut().unwrap().entries {
        entry.parent = Some(remote_root);
    }
    let epoch = SourceEpoch::random_v4(66, 77);
    let first_chain = [0x66; 32];
    let head_chain = [0x77; 32];
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(source, epoch, 2, head_chain, 0)
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    let child_first = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch),
        after_seq: 0,
        after_chain: CHAIN_GENESIS,
        through_seq: 1,
        through_chain: first_chain,
        head_seq: 2,
        rows: vec![child],
    };
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &child_first)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    let placeholder: i64 = central
        .catalog
        .with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT placeholder FROM sync_replica_rows
                 WHERE source_id = ?1 AND remote_object_id = ?2",
                [source.0, remote_root.0],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(placeholder, 1);
    let parent = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch),
        after_seq: 1,
        after_chain: first_chain,
        through_seq: 2,
        through_chain: head_chain,
        head_seq: 2,
        rows: vec![root.clone()],
    };
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &parent)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    let new_root = central
        .catalog
        .with_reader(|conn| {
            Ok(ObjectId(conn.query_row(
                "SELECT local_object_id FROM sync_replica_rows
                 WHERE source_id = ?1 AND remote_object_id = ?2",
                [source.0, remote_root.0],
                |row| row.get(0),
            )?))
        })
        .unwrap();
    assert_eq!(
        new_root, old_root,
        "stable native identity reuses the local row"
    );
    assert_eq!(
        central
            .catalog
            .get_source(source)
            .unwrap()
            .unwrap()
            .root_object_id,
        Some(new_root)
    );

    // Generic/path-derived sources may not have a native identity to remap.
    // Their new root gets a new local row and must replace root_object_id.
    let remote_root_2 = ObjectId(remote_root.0 + 1);
    root.object = remote_root_2;
    root.seq = 1;
    root.image.as_mut().unwrap().object.id = remote_root_2;
    root.image.as_mut().unwrap().object.native = None;
    let epoch_2 = SourceEpoch::random_v4(88, 99);
    let head_chain_2 = [0x88; 32];
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(source, epoch_2, 1, head_chain_2, 0)
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    let snapshot_2 = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch_2),
        after_seq: 0,
        after_chain: CHAIN_GENESIS,
        through_seq: 1,
        through_chain: head_chain_2,
        head_seq: 1,
        rows: vec![root],
    };
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &snapshot_2)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    let newest_root = central
        .catalog
        .with_reader(|conn| {
            Ok(ObjectId(conn.query_row(
                "SELECT local_object_id FROM sync_replica_rows
                 WHERE source_id = ?1 AND remote_object_id = ?2",
                [source.0, remote_root_2.0],
                |row| row.get(0),
            )?))
        })
        .unwrap();
    assert_ne!(newest_root, old_root);
    assert_eq!(
        central
            .catalog
            .get_source(source)
            .unwrap()
            .unwrap()
            .root_object_id,
        Some(newest_root)
    );
}

#[test]
fn a_rebuilt_epoch_can_swap_reused_remote_ids_without_changing_local_identity() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let one_remote = node.id("a/one.txt");
    let three_remote = node.id("c/three.txt");
    let one_local = central
        .catalog
        .resolve_relative(source, "a/one.txt")
        .unwrap()
        .unwrap();
    let three_local = central
        .catalog
        .resolve_relative(source, "c/three.txt")
        .unwrap()
        .unwrap();

    let mut snapshot = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let epoch = SourceEpoch::random_v4(101, 202);
    snapshot.epoch = SyncEpoch::from_source_epoch(epoch);
    for row in &mut snapshot.rows {
        let replacement = if row.object == one_remote {
            Some(three_remote)
        } else if row.object == three_remote {
            Some(one_remote)
        } else {
            None
        };
        if let Some(replacement) = replacement {
            row.object = replacement;
            row.image.as_mut().unwrap().object.id = replacement;
        }
    }
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(source, epoch, snapshot.head_seq, snapshot.through_chain, 0,)
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &snapshot)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));

    let (mapped_one, mapped_three) = central
        .catalog
        .with_reader(|conn| {
            let mut mapping = conn.prepare(
                "SELECT local_object_id FROM sync_replica_rows
                 WHERE source_id = ?1 AND remote_object_id = ?2",
            )?;
            Ok((
                ObjectId(mapping.query_row([source.0, one_remote.0], |row| row.get(0))?),
                ObjectId(mapping.query_row([source.0, three_remote.0], |row| row.get(0))?),
            ))
        })
        .unwrap();
    assert_eq!(mapped_one, three_local);
    assert_eq!(mapped_three, one_local);
    assert_eq!(central.image(source), node.image());
}

#[test]
fn a_child_first_reference_follows_a_reused_real_parent_mapping() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let reused_remote = node.id("a/one.txt");
    let mut original = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap()
        .rows;
    let root_position = original
        .iter()
        .position(|row| {
            row.image
                .as_ref()
                .is_some_and(|image| image.entries.iter().any(|entry| entry.parent.is_none()))
        })
        .expect("source root row");
    let mut root = original.swap_remove(root_position);
    let old_root_remote = root.object;
    root.object = reused_remote;
    root.seq = 2;
    root.image.as_mut().unwrap().object.id = reused_remote;

    let child_position = original
        .iter()
        .position(|row| row.object == node.id("a"))
        .expect("child directory row");
    let mut child = original.swap_remove(child_position);
    child.seq = 1;
    for entry in &mut child.image.as_mut().unwrap().entries {
        assert_eq!(entry.parent, Some(old_root_remote));
        entry.parent = Some(reused_remote);
    }

    let epoch = SourceEpoch::random_v4(505, 606);
    let first_chain = [0x51; 32];
    let head_chain = [0x52; 32];
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(source, epoch, 2, head_chain, 0)
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    let child_first = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch),
        after_seq: 0,
        after_chain: CHAIN_GENESIS,
        through_seq: 1,
        through_chain: first_chain,
        head_seq: 2,
        rows: vec![child],
    };
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &child_first)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    let parent = SyncBatch {
        source_id: node.source,
        epoch: SyncEpoch::from_source_epoch(epoch),
        after_seq: 1,
        after_chain: first_chain,
        through_seq: 2,
        through_chain: head_chain,
        head_seq: 2,
        rows: vec![root],
    };
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &parent)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    let root_local = central
        .catalog
        .get_source(source)
        .unwrap()
        .unwrap()
        .root_object_id
        .expect("replica root");
    let child_local = central
        .catalog
        .resolve_relative(source, "a")
        .unwrap()
        .expect("child follows remapped parent");
    let parent_local = central
        .catalog
        .with_reader(|conn| {
            Ok(ObjectId(conn.query_row(
                "SELECT parent_id FROM entries
                 WHERE source_id = ?1 AND object_id = ?2 AND deleted_at IS NULL",
                [source.0, child_local.0],
                |row| row.get(0),
            )?))
        })
        .unwrap();
    assert_eq!(parent_local, root_local);
}

#[test]
fn unused_child_first_placeholders_are_removed_when_an_epoch_is_retired() {
    let node = Node::new();
    let central = Central::new();
    let source = central.source_for(&node);
    node.touch("a", 0, 1_700_000_000);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap(),
        HelloOutcome::Resume { .. }
    ));
    let mut after = 0;
    loop {
        let batch = node.catalog.sync_rows_after(node.source, after, 1).unwrap();
        let through = batch.through_seq;
        assert!(matches!(
            central.catalog.replica_apply_batch(source, &batch).unwrap(),
            BatchOutcome::Applied { .. }
        ));
        let placeholders: i64 = central
            .catalog
            .with_reader(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM sync_replica_rows WHERE source_id = ?1 AND placeholder = 1",
                    [source.0],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        if placeholders > 0 {
            break;
        }
        after = through;
        assert!(
            after < state.head_seq,
            "fixture never produced a placeholder"
        );
    }

    node.catalog.sync_disable(node.source).unwrap();
    std::fs::remove_dir_all(node.root.join("a")).unwrap();
    node.scan();
    node.catalog.sync_enable(node.source, None).unwrap();
    while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
    converge(&node, &central, 2);
    let placeholders: i64 = central
        .catalog
        .with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM sync_replica_rows WHERE source_id = ?1 AND placeholder = 1",
                [source.0],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(placeholders, 0);
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
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries
            .iter()
            .map(|e| record_digest(e.object, e.generation, e.deleted, &e.image_hash)),
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
    match central
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
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => {
            assert!(reason.contains("outstanding request"), "{reason}")
        }
        other => panic!("replayed repair was accepted: {other:?}"),
    }
    node.catalog
        .sync_acknowledge(node.source, state.epoch, CENTRAL, state.head_seq)
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
fn compacted_metadata_only_changes_are_visible_to_merkle_repair() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let before_cursor = central.applied_seq(source);
    let object = node.id("a/one.txt");
    let before = node.catalog.get_object(object).unwrap().unwrap();
    let changed_attributes = FileAttributes(before.attributes.0 ^ 1);

    node.catalog
        .with_writer(|conn| {
            conn.execute("DELETE FROM sync_consumers", [])?;
            Ok(())
        })
        .unwrap();
    node.set_attributes("a/one.txt", changed_attributes);
    let changed = node.catalog.get_object(object).unwrap().unwrap();
    assert_eq!(changed.generation, before.generation);
    assert_eq!(changed.attributes, changed_attributes);
    node.catalog.sync_collect(node.source, 100).unwrap();
    let (state, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    assert!(state.compacted_through > before_cursor);

    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap(),
        HelloOutcome::Resume {
            requires_repair: true,
            ..
        }
    ));
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries.iter().map(|entry| {
            record_digest(
                entry.object,
                entry.generation,
                entry.deleted,
                &entry.image_hash,
            )
        }),
    );
    let leaves = match central
        .catalog
        .replica_repair_offer(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &tree.leaf_hashes(),
        )
        .unwrap()
    {
        eidos_catalog::replica::RepairOfferOutcome::Request { leaves, .. } => leaves,
        other => panic!("metadata-only divergence was not offered for repair: {other:?}"),
    };
    assert!(!leaves.is_empty());
    let wanted: BTreeSet<_> = leaves.iter().copied().collect();
    let objects: Vec<_> = entries
        .iter()
        .filter(|entry| wanted.contains(&leaf_index(leaf_bits, entry.object)))
        .map(|entry| entry.object)
        .collect();
    let (_, rows) = node
        .catalog
        .sync_rows_for_objects(node.source, &objects)
        .unwrap();
    assert!(matches!(
        central
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
            .unwrap(),
        RepairOutcome::Applied { .. }
    ));
    let local = central
        .catalog
        .resolve_relative(source, "a/one.txt")
        .unwrap()
        .unwrap();
    let repaired = central.catalog.get_object(local).unwrap().unwrap();
    assert_eq!(repaired.attributes, changed_attributes);
    assert_eq!(repaired.generation, before.generation);
}

#[test]
fn repair_replay_is_chain_bound_and_rejects_duplicate_objects() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    let object = node.id("a/one.txt");
    let (_, rows) = node
        .catalog
        .sync_rows_for_objects(node.source, &[object])
        .unwrap();
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let leaves = [leaf_index(leaf_bits, object)];

    // An authoritative response is unusable until the catalog has durably
    // selected its exact leaf scope from a matching offer.
    match central
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
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => {
            assert!(reason.contains("outstanding request"), "{reason}")
        }
        other => panic!("{other:?}"),
    }

    // Force exactly the object's leaf to differ, creating the durable
    // outstanding request used by the remaining checks.
    let (_, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    let mut hashes = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries.iter().map(|entry| {
            record_digest(
                entry.object,
                entry.generation,
                entry.deleted,
                &entry.image_hash,
            )
        }),
    )
    .leaf_hashes();
    hashes[leaves[0] as usize][0] ^= 0xff;
    let requested = central
        .catalog
        .replica_repair_offer(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &hashes,
        )
        .unwrap();
    assert!(matches!(
        requested,
        eidos_catalog::replica::RepairOfferOutcome::Request {
            leaves: requested_leaves,
            ..
        } if requested_leaves.as_slice() == leaves
    ));

    let mut oversized = rows[0].clone();
    let entry = oversized.image.as_ref().unwrap().entries[0].clone();
    oversized.image.as_mut().unwrap().entries = vec![entry; 100_001];
    match central
        .catalog
        .replica_apply_repair(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &leaves,
            &[oversized],
        )
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => assert!(reason.contains("malformed"), "{reason}"),
        other => panic!("{other:?}"),
    }

    let mut wrong_chain = state.head_chain;
    wrong_chain[0] ^= 0xff;
    match central
        .catalog
        .replica_apply_repair(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            wrong_chain,
            leaf_bits,
            &leaves,
            &rows,
        )
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => assert!(reason.contains("history fork"), "{reason}"),
        other => panic!("{other:?}"),
    }

    let wrong_leaf = (leaves[0] + 1) % (1 << leaf_bits);
    match central
        .catalog
        .replica_apply_repair(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &[wrong_leaf],
            &[],
        )
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => assert!(reason.contains("outstanding"), "{reason}"),
        other => panic!("{other:?}"),
    }

    let mut conflicting = rows[0].clone();
    conflicting.image.as_mut().unwrap().object.size = 999;
    match central
        .catalog
        .replica_apply_repair(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &leaves,
            &[rows[0].clone(), conflicting],
        )
        .unwrap()
    {
        RepairOutcome::Rejected { reason } => assert!(reason.contains("malformed"), "{reason}"),
        other => panic!("{other:?}"),
    }
    let local = central
        .catalog
        .resolve_relative(source, "a/one.txt")
        .unwrap()
        .unwrap();
    assert_eq!(
        central.catalog.get_object(local).unwrap().unwrap().size,
        100
    );
}

#[test]
fn repair_that_finishes_an_epoch_resync_retires_the_previous_epoch() {
    let node = Node::new();
    let mut central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    node.catalog.sync_enable(node.source, None).unwrap();
    while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
    let state = node.catalog.sync_source(node.source).unwrap().unwrap();
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    let first = node.catalog.sync_rows_after(node.source, 0, 1).unwrap();
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &first).unwrap(),
        BatchOutcome::Applied { .. }
    ));
    assert!(central
        .catalog
        .replica_source(source)
        .unwrap()
        .unwrap()
        .resync_target
        .is_some());
    let partial = central.catalog.source_completeness(source).unwrap();
    assert_eq!(partial.state, SourceState::Reconciling);
    assert!(!partial.metadata_complete);

    let (state, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let source_tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries.iter().map(|entry| {
            record_digest(
                entry.object,
                entry.generation,
                entry.deleted,
                &entry.image_hash,
            )
        }),
    );
    let request = central
        .catalog
        .replica_repair_offer(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            state.head_chain,
            leaf_bits,
            &source_tree.leaf_hashes(),
        )
        .unwrap();
    let leaves = match request {
        eidos_catalog::replica::RepairOfferOutcome::Request { leaves, .. } => leaves,
        other => panic!("{other:?}"),
    };
    let wanted: BTreeSet<_> = leaves.iter().copied().collect();
    let objects: Vec<_> = entries
        .iter()
        .filter(|entry| wanted.contains(&leaf_index(leaf_bits, entry.object)))
        .map(|entry| entry.object)
        .collect();
    let (_, rows) = node
        .catalog
        .sync_rows_for_objects(node.source, &objects)
        .unwrap();
    let split = leaves.len() / 2;
    let (first_leaves, final_leaves) = leaves.split_at(split);
    let first_scope: BTreeSet<_> = first_leaves.iter().copied().collect();
    let (first_rows, final_rows): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| first_scope.contains(&leaf_index(leaf_bits, row.object)));
    assert!(matches!(
        central
            .catalog
            .replica_apply_repair_part(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                leaf_bits,
                first_leaves,
                &first_rows,
                false,
            )
            .unwrap(),
        RepairOutcome::Staged {
            remaining_leaves,
            ..
        } if remaining_leaves == final_leaves.len() as u64
    ));
    assert!(
        !central
            .catalog
            .source_completeness(source)
            .unwrap()
            .metadata_complete
    );
    central.reopen();
    assert!(matches!(
        central
            .catalog
            .replica_apply_repair_part(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                leaf_bits,
                final_leaves,
                &final_rows,
                true,
            )
            .unwrap(),
        RepairOutcome::Applied { .. }
    ));
    assert_eq!(
        central
            .catalog
            .replica_source(source)
            .unwrap()
            .unwrap()
            .resync_target,
        None
    );
    assert!(
        central
            .catalog
            .source_completeness(source)
            .unwrap()
            .metadata_complete
    );
    assert_eq!(central.image(source), node.image());
    let replica_tree =
        MerkleTree::with_leaf_bits(leaf_bits, central.catalog.replica_digests(source).unwrap());
    assert_eq!(replica_tree.leaf_hashes(), source_tree.leaf_hashes());
}

#[test]
fn an_outstanding_repair_keeps_a_later_batch_reconciling() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    let applied = central.applied_seq(source);
    node.touch("a/one.txt", 123, 1_700_000_123);
    let (state, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap(),
        HelloOutcome::Resume { .. }
    ));
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries.iter().map(|entry| {
            record_digest(
                entry.object,
                entry.generation,
                entry.deleted,
                &entry.image_hash,
            )
        }),
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
    assert!(matches!(
        request,
        eidos_catalog::replica::RepairOfferOutcome::Request { ref leaves, .. }
            if !leaves.is_empty()
    ));
    let batch = node
        .catalog
        .sync_rows_after(node.source, applied, u32::MAX)
        .unwrap();
    assert!(matches!(
        central.catalog.replica_apply_batch(source, &batch).unwrap(),
        BatchOutcome::Applied { .. }
    ));
    let completeness = central.catalog.source_completeness(source).unwrap();
    assert_eq!(completeness.state, SourceState::Reconciling);
    assert!(!completeness.metadata_complete);
    assert!(central.catalog.replica_retire_source(source).unwrap());
    assert!(central.catalog.replica_source(source).unwrap().is_none());
    let (repairs, mappings): (i64, i64) = central
        .catalog
        .with_reader(|conn| {
            Ok((
                conn.query_row(
                    "SELECT COUNT(*) FROM sync_replica_repairs WHERE source_id = ?1",
                    [source.0],
                    |row| row.get(0),
                )?,
                conn.query_row(
                    "SELECT COUNT(*) FROM sync_replica_rows WHERE source_id = ?1",
                    [source.0],
                    |row| row.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!((repairs, mappings), (0, 0));
    assert_eq!(
        central.catalog.get_source(source).unwrap().unwrap().state,
        SourceState::Retired
    );
}

#[test]
fn snapshot_fallback_preserves_rows_already_staged_by_repair() {
    let node = Node::new();
    let mut central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);
    node.catalog.sync_enable(node.source, None).unwrap();
    while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
    let (state, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                0,
            )
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries.iter().map(|entry| {
            record_digest(
                entry.object,
                entry.generation,
                entry.deleted,
                &entry.image_hash,
            )
        }),
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
    let requested = match request {
        eidos_catalog::replica::RepairOfferOutcome::Request { leaves, .. } => leaves,
        other => panic!("{other:?}"),
    };
    let staged_leaf = leaf_index(leaf_bits, entries[0].object);
    assert!(requested.contains(&staged_leaf));
    let objects: Vec<_> = entries
        .iter()
        .filter(|entry| leaf_index(leaf_bits, entry.object) == staged_leaf)
        .map(|entry| entry.object)
        .collect();
    let (_, staged_rows) = node
        .catalog
        .sync_rows_for_objects(node.source, &objects)
        .unwrap();
    assert!(!staged_rows.is_empty());
    assert!(matches!(
        central
            .catalog
            .replica_apply_repair_part(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                leaf_bits,
                &[staged_leaf],
                &staged_rows,
                false,
            )
            .unwrap(),
        RepairOutcome::Staged { .. }
    ));

    central.reopen();
    let mut wrong_chain = state.head_chain;
    wrong_chain[0] ^= 0xff;
    match central
        .catalog
        .replica_admit_hello(
            source,
            state.epoch.to_source_epoch(),
            state.head_seq,
            wrong_chain,
            0,
        )
        .unwrap()
    {
        HelloOutcome::Rejected { reason } => assert!(reason.contains("history fork"), "{reason}"),
        other => panic!("{other:?}"),
    }
    let snapshot = node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .unwrap();
    let mut forked_snapshot = snapshot.clone();
    forked_snapshot.through_chain = wrong_chain;
    match central
        .catalog
        .replica_apply_batch(source, &forked_snapshot)
        .unwrap()
    {
        BatchOutcome::Rejected { reason } => assert!(reason.contains("history fork"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        central
            .catalog
            .replica_apply_batch(source, &snapshot)
            .unwrap(),
        BatchOutcome::Applied { .. }
    ));
    assert_eq!(central.image(source), node.image());
    assert!(
        central
            .catalog
            .source_completeness(source)
            .unwrap()
            .metadata_complete
    );
}

#[test]
fn a_compacted_new_epoch_can_install_its_first_snapshot_by_repair() {
    let node = Node::new();
    let central = Central::new();
    converge(&node, &central, 8);
    let source = central.source_for(&node);

    node.catalog.sync_enable(node.source, None).unwrap();
    while !node.catalog.sync_backfill(node.source, 2).unwrap().done {}
    node.delete("a/b/two.txt");
    let collected = node.catalog.sync_collect(node.source, 100).unwrap();
    assert!(collected.compacted_through > 0);
    let (state, entries) = node.catalog.sync_ledger_entries(node.source).unwrap();
    assert!(matches!(
        central
            .catalog
            .replica_admit_hello(
                source,
                state.epoch.to_source_epoch(),
                state.head_seq,
                state.head_chain,
                state.compacted_through,
            )
            .unwrap(),
        HelloOutcome::FullResync { .. }
    ));
    assert!(node
        .catalog
        .sync_rows_after(node.source, 0, u32::MAX)
        .is_err());

    let leaf_bits = MIN_FLEET_LEAF_BITS;
    let tree = MerkleTree::with_leaf_bits(
        leaf_bits,
        entries.iter().map(|entry| {
            record_digest(
                entry.object,
                entry.generation,
                entry.deleted,
                &entry.image_hash,
            )
        }),
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
    assert_eq!(leaves.len(), 1usize << leaf_bits);
    let objects: Vec<_> = entries.iter().map(|entry| entry.object).collect();
    let (_, rows) = node
        .catalog
        .sync_rows_for_objects(node.source, &objects)
        .unwrap();
    assert!(matches!(
        central
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
            .unwrap(),
        RepairOutcome::Applied { .. }
    ));
    assert_eq!(central.image(source), node.image());
    let replica = central.catalog.replica_source(source).unwrap().unwrap();
    assert_eq!(replica.admission.epoch, state.epoch.to_source_epoch());
    assert_eq!(replica.resync_target, None);
}

#[test]
fn replica_sources_preserve_the_origins_case_sensitivity() {
    let node = Node::new();
    let central = Central::new();
    let mut descriptor = node.descriptor();
    descriptor.case_sensitive = true;
    let source = central
        .catalog
        .replica_ensure_source(
            &RemoteNode {
                node_id: NODE,
                name: "node-a".into(),
                platform: "macos".into(),
            },
            &descriptor,
            node.epoch(),
        )
        .unwrap()
        .source_id;
    assert!(central.catalog.is_source_case_sensitive(source).unwrap());
}

#[test]
fn two_nodes_with_the_same_source_name_and_path_stay_distinct() {
    let a = Node::new();
    let b = Node::new();
    let central = Central::new();
    converge(&a, &central, 8);
    let node_b = RemoteNode {
        node_id: [0x0B; 16],
        name: "node-a".into(),
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
    assert!(names.contains("node-a/docs#2"));
    let hosts: BTreeSet<_> = central
        .catalog
        .list_sources()
        .unwrap()
        .into_iter()
        .map(|s| s.host_id)
        .collect();
    assert_eq!(hosts.len(), 2);
}

#[test]
fn node_id_text_rejects_non_ascii_without_panicking() {
    let malformed = format!("a{}x", "é".repeat(15));
    assert_eq!(malformed.len(), 32);
    assert_eq!(NodeId::parse_hex(&malformed), None);
}

#[test]
fn one_certificate_fingerprint_cannot_enroll_as_two_node_ids() {
    let central = Central::new();
    let first = FleetPeer {
        node_id: NodeId([1; 16]),
        name: "first".into(),
        role: PeerRole::Node,
        fingerprint: [9; 32],
        endpoint: None,
        enabled: true,
        enrolled_at: UnixNanos(1),
        last_seen_at: None,
        last_error: None,
    };
    central.catalog.fleet_upsert_peer(&first).unwrap();
    let mut collision = first.clone();
    collision.node_id = NodeId([2; 16]);
    collision.name = "second".into();
    assert!(central.catalog.fleet_upsert_peer(&collision).is_err());
    assert_eq!(
        central
            .catalog
            .fleet_peer_by_fingerprint(&first.fingerprint)
            .unwrap()
            .unwrap()
            .node_id,
        first.node_id
    );
}

#[test]
fn the_history_chain_migration_reincarnates_existing_sync_ledgers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    let predecessor = eidos_catalog::schema::MIGRATIONS.len() - 1;
    for (index, (description, sql)) in eidos_catalog::schema::MIGRATIONS
        .iter()
        .take(predecessor)
        .enumerate()
    {
        let version = index as i64 + 1;
        let tx = conn.transaction().unwrap();
        tx.execute_batch(sql).unwrap();
        tx.execute(
            "INSERT INTO schema_migrations (version, description, applied_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![version, description, version],
        )
        .unwrap();
        tx.pragma_update(None, "user_version", version).unwrap();
        tx.commit().unwrap();
    }
    conn.execute(
        "INSERT INTO hosts (host_id, name, platform, created_at) VALUES (1, 'old', 'windows', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sources (source_id, host_id, name, kind, root_path, created_at, updated_at)
         VALUES (1, 1, 'old-source', 'windows_local', 'C:\\old', 1, 1)",
        [],
    )
    .unwrap();
    let old_epoch = [3u8; 16];
    conn.execute(
        "INSERT INTO sync_sources (source_id, epoch, head_seq, compacted_through, backfill_after,
             ready, created_at, updated_at) VALUES (1, ?1, 7, 2, 99, 1, 1, 1)",
        [old_epoch.as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sync_rows (source_id, object_id, seq, generation, deleted)
         VALUES (1, 99, 7, 4, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sync_consumers (source_id, consumer_id, watermark, updated_at)
         VALUES (1, ?1, 7, 1)",
        [CENTRAL.as_slice()],
    )
    .unwrap();
    drop(conn);

    let catalog = Catalog::open(path).unwrap();
    let state = catalog.sync_source(SourceId(1)).unwrap().unwrap();
    assert_ne!(state.epoch.0, old_epoch);
    assert_eq!(state.head_seq, 0);
    assert_eq!(state.compacted_through, 0);
    assert_eq!(state.backfill_after, ObjectId(0));
    assert!(!state.ready);
    assert_eq!(state.head_chain, CHAIN_GENESIS);
    assert_eq!(
        catalog.sync_chain_at(SourceId(1), 0).unwrap(),
        Some(CHAIN_GENESIS)
    );
    assert!(catalog.sync_consumers(SourceId(1)).unwrap().is_empty());
    let rows: i64 = catalog
        .with_reader(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM sync_rows", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(rows, 0);
}
