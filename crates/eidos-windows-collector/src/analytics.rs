//! Change analytics over a native journal: turns raw records into keyed,
//! bucketed `LogicalChange`s and per-interval summaries. Platform-neutral
//! so the classification, pairing, and coalescing arithmetic are tested
//! without a volume.
//!
//! Identity: an object is `(volume, file reference number)`, a subtree is
//! its parent's reference number. Names are used only to pick an extension
//! bucket and to key a delete/recreate lookup, then dropped.

use eidos_observe::{
    bucket_age, bucket_depth, bucket_extension, bucket_size, ChangeOperation, CoalescingWindows,
    CountBucket, DepthBucket, ExtensionBucket, Histogram, LogicalChange, ObjectToken,
    OperationCounts, RateSummary, ReasonSummary, SizeBucket, StudyKey, TimeAnchor,
};
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

// USN reason bits (from winioctl.h); kept here so the analyzer compiles on
// every platform.
pub const REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const REASON_NAMED_DATA_OVERWRITE: u32 = 0x0000_0010;
pub const REASON_NAMED_DATA_EXTEND: u32 = 0x0000_0020;
pub const REASON_NAMED_DATA_TRUNCATION: u32 = 0x0000_0040;
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
pub const REASON_STREAM_CHANGE: u32 = 0x0020_0000;
pub const REASON_CLOSE: u32 = 0x8000_0000;

const DATA_MASK: u32 = REASON_DATA_OVERWRITE | REASON_DATA_EXTEND | REASON_DATA_TRUNCATION;
const STREAM_MASK: u32 = REASON_NAMED_DATA_OVERWRITE
    | REASON_NAMED_DATA_EXTEND
    | REASON_NAMED_DATA_TRUNCATION
    | REASON_STREAM_CHANGE;

/// One raw journal record as the analyzer sees it. `name` is borrowed and
/// never stored.
#[derive(Debug, Clone)]
pub struct RecordView<'a> {
    pub usn: i64,
    pub frn: u128,
    pub parent_frn: u128,
    pub reason: u32,
    pub is_directory: bool,
    pub name: &'a str,
    pub timestamp_ns: i64,
}

/// Facts the lane can look up cheaply for a closed object; both are
/// optional so a saturated batch can skip them.
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectFacts {
    pub size: Option<u64>,
    pub depth: Option<usize>,
}

const TRACKED: usize = 65_536;
const MAX_DISTINCT_PER_WINDOW: usize = 2_000_000;
const HOT_EDITS: u32 = 10;
const WINDOWS_S: [u64; 5] = [1, 10, 60, 600, 3600];

struct CoalescingWindow {
    width_s: u64,
    started_s: i64,
    objects: HashSet<u128>,
    saturated: bool,
    /// Rows a batcher of this width would have emitted during the current
    /// summary interval.
    emitted: u64,
}

impl CoalescingWindow {
    fn new(width_s: u64, now_s: i64) -> Self {
        Self {
            width_s,
            started_s: now_s,
            objects: HashSet::new(),
            saturated: false,
            emitted: 0,
        }
    }

    fn observe(&mut self, frn: u128, now_s: i64) {
        self.roll(now_s);
        if self.objects.len() >= MAX_DISTINCT_PER_WINDOW {
            if !self.saturated {
                self.saturated = true;
            }
            // Beyond the bound every change is counted as its own row,
            // which over-estimates rows rather than hiding them.
            self.emitted += 1;
            return;
        }
        self.objects.insert(frn);
    }

    fn roll(&mut self, now_s: i64) {
        if now_s - self.started_s >= self.width_s as i64 {
            self.emitted += self.objects.len() as u64;
            self.objects.clear();
            self.saturated = false;
            self.started_s = now_s;
        }
    }

    fn take_emitted(&mut self) -> u64 {
        std::mem::take(&mut self.emitted)
    }
}

pub struct ChangeAnalyzer {
    volume_id: Vec<u8>,
    interval_started_s: i64,
    pending_rename: Option<(u128, u128, i64)>,
    edit_counts: LruCache<u128, u32>,
    recent_deletes: LruCache<[u8; 32], i64>,
    fan_out: LruCache<u128, u32>,
    per_second: HashMap<i64, u64>,
    operations: OperationCounts,
    records: u64,
    logical_changes: u64,
    coalesced: u64,
    close_records: u64,
    intermediate_records: u64,
    directory_records: u64,
    tombstones: u64,
    recreates: u64,
    hot_objects: HashSet<u128>,
    directories: HashSet<u128>,
    reasons: HashMap<u32, u64>,
    extensions: HashMap<ExtensionBucket, u64>,
    sizes: HashMap<SizeBucket, u64>,
    depths: HashMap<DepthBucket, u64>,
    max_backlog_s: u64,
    backlog_ms: Histogram,
    windows: Vec<CoalescingWindow>,
}

/// Everything one summary interval produces.
pub struct IntervalOutput {
    pub rate: RateSummary,
    pub reasons: ReasonSummary,
    /// Time from journal timestamp to processing, for the feed-health record.
    pub backlog_ms: Histogram,
    pub logical_changes: u64,
    pub coalesced: u64,
}

impl ChangeAnalyzer {
    pub fn new(volume_id: &[u8], now_s: i64) -> Self {
        let cap = NonZeroUsize::new(TRACKED).expect("nonzero");
        Self {
            volume_id: volume_id.to_vec(),
            interval_started_s: now_s,
            pending_rename: None,
            edit_counts: LruCache::new(cap),
            recent_deletes: LruCache::new(cap),
            fan_out: LruCache::new(cap),
            per_second: HashMap::new(),
            operations: OperationCounts::default(),
            records: 0,
            logical_changes: 0,
            coalesced: 0,
            close_records: 0,
            intermediate_records: 0,
            directory_records: 0,
            tombstones: 0,
            recreates: 0,
            hot_objects: HashSet::new(),
            directories: HashSet::new(),
            reasons: HashMap::new(),
            extensions: HashMap::new(),
            sizes: HashMap::new(),
            depths: HashMap::new(),
            max_backlog_s: 0,
            backlog_ms: Histogram::new(),
            windows: WINDOWS_S
                .iter()
                .map(|w| CoalescingWindow::new(*w, now_s))
                .collect(),
        }
    }

    pub fn volume_id(&self) -> &[u8] {
        &self.volume_id
    }

    pub fn object_token(&self, key: &StudyKey, frn: u128) -> ObjectToken {
        key.token("object", &self.identity(frn))
    }

    fn identity(&self, frn: u128) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.volume_id.len() + 16);
        bytes.extend_from_slice(&self.volume_id);
        bytes.extend_from_slice(&frn.to_le_bytes());
        bytes
    }

    /// Whether this record needs object facts (size, depth) before
    /// `observe`; lets the lane skip lookups for intermediate records.
    pub fn wants_facts(record: &RecordView<'_>) -> bool {
        record.reason & REASON_CLOSE != 0
            && !record.is_directory
            && record.reason & REASON_FILE_DELETE == 0
    }

    /// Feed one record; returns a logical change for close records.
    pub fn observe(
        &mut self,
        key: &StudyKey,
        record: &RecordView<'_>,
        facts: ObjectFacts,
        now: TimeAnchor,
    ) -> Option<LogicalChange> {
        let now_s = now.utc_ns / 1_000_000_000;
        self.records += 1;
        *self.reasons.entry(record.reason).or_default() += 1;
        if record.is_directory {
            self.directory_records += 1;
        }
        let backlog_ns = now.utc_ns.saturating_sub(record.timestamp_ns).max(0) as u64;
        self.backlog_ms.observe(backlog_ns / 1_000_000);
        self.max_backlog_s = self.max_backlog_s.max(backlog_ns / 1_000_000_000);

        if record.reason & REASON_RENAME_OLD_NAME != 0 && record.reason & REASON_CLOSE == 0 {
            self.pending_rename = Some((record.frn, record.parent_frn, record.usn));
        }
        if record.reason & REASON_CLOSE == 0 {
            self.intermediate_records += 1;
            self.coalesced += 1;
            return None;
        }
        self.close_records += 1;
        self.logical_changes += 1;
        *self.per_second.entry(now_s).or_default() += 1;

        let operation = classify(record.reason);
        match operation {
            ChangeOperation::Create => self.operations.creates += 1,
            ChangeOperation::Update => self.operations.updates += 1,
            ChangeOperation::Delete => self.operations.deletes += 1,
            ChangeOperation::Rename => self.operations.renames += 1,
            ChangeOperation::Metadata => self.operations.metadata += 1,
            ChangeOperation::HardLink => self.operations.hard_links += 1,
            ChangeOperation::Stream => self.operations.streams += 1,
        }
        if record.is_directory {
            self.directories.insert(record.frn);
        }
        for window in &mut self.windows {
            window.observe(record.frn, now_s);
        }

        let edits = {
            let count = self.edit_counts.get_or_insert_mut(record.frn, || 0);
            *count += 1;
            *count
        };
        if edits >= HOT_EDITS {
            self.hot_objects.insert(record.frn);
        }
        let fan_out = {
            let count = self.fan_out.get_or_insert_mut(record.parent_frn, || 0);
            *count += 1;
            *count
        };

        let name_key = self.name_key(key, record.parent_frn, record.name);
        let delete_recreate_age = match operation {
            ChangeOperation::Delete => {
                self.tombstones += 1;
                self.recent_deletes.put(name_key, now.utc_ns);
                None
            }
            ChangeOperation::Create => self.recent_deletes.pop(&name_key).map(|deleted| {
                self.recreates += 1;
                bucket_age((now.utc_ns.saturating_sub(deleted).max(0) / 1_000_000_000) as u64)
            }),
            _ => None,
        };
        let rename_pair = match (operation, self.pending_rename.take()) {
            (ChangeOperation::Rename, Some((frn, old_parent, _))) if frn == record.frn => {
                Some(key.token("subtree", &self.identity(old_parent)))
            }
            (_, pending) => {
                // Keep an unrelated pending rename for its own close record.
                if let Some(pending) = pending {
                    if pending.0 != record.frn {
                        self.pending_rename = Some(pending);
                    }
                }
                None
            }
        };
        let extension = if record.is_directory {
            ExtensionBucket::None
        } else {
            bucket_extension(record.name.rsplit_once('.').map(|(_, ext)| ext))
        };
        let size = match (record.is_directory, facts.size) {
            (true, _) => SizeBucket::Zero,
            (false, Some(size)) => bucket_size(size),
            (false, None) => SizeBucket::Unknown,
        };
        let depth = facts.depth.map(bucket_depth).unwrap_or(DepthBucket::Root);
        *self.extensions.entry(extension).or_default() += 1;
        *self.sizes.entry(size).or_default() += 1;
        *self.depths.entry(depth).or_default() += 1;

        Some(LogicalChange {
            at: now,
            object: self.object_token(key, record.frn),
            subtree: key.token("subtree", &self.identity(record.parent_frn)),
            operation,
            rename_pair,
            size,
            extension,
            depth,
            edit_count: CountBucket::from(edits as u64),
            delete_recreate_age,
            fan_out: CountBucket::from(fan_out as u64),
            backlog_age: bucket_age(backlog_ns / 1_000_000_000),
        })
    }

    fn name_key(&self, key: &StudyKey, parent_frn: u128, name: &str) -> [u8; 32] {
        let mut hasher = key.hasher("name");
        hasher.update(&self.identity(parent_frn));
        hasher.update(name.to_lowercase().as_bytes());
        hasher.finish_digest()
    }

    /// Close the interval and return its summaries; counters reset.
    pub fn flush(&mut self, key: &StudyKey, now: TimeAnchor) -> IntervalOutput {
        let now_s = now.utc_ns / 1_000_000_000;
        let interval_s = (now_s - self.interval_started_s).max(1) as u32;
        let volume = key.token("volume", &self.volume_id);
        let mut per_second = Histogram::new();
        for count in self.per_second.values() {
            per_second.observe(*count);
        }
        let idle_seconds = (interval_s as u64).saturating_sub(self.per_second.len() as u64);
        for _ in 0..idle_seconds {
            per_second.observe(0);
        }
        for window in &mut self.windows {
            window.roll(now_s);
        }
        let mut emitted = self.windows.iter_mut().map(CoalescingWindow::take_emitted);
        let coalesced_windows = CoalescingWindows {
            w1s: emitted.next().unwrap_or(0),
            w10s: emitted.next().unwrap_or(0),
            w60s: emitted.next().unwrap_or(0),
            w600s: emitted.next().unwrap_or(0),
            w3600s: emitted.next().unwrap_or(0),
        };
        let mut combinations: Vec<(u32, u64)> = self.reasons.drain().collect();
        combinations.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        combinations.truncate(48);

        let rate = RateSummary {
            at: now.clone(),
            volume: volume.clone(),
            interval_s,
            records: self.records,
            logical_changes: self.logical_changes,
            per_second,
            operations: std::mem::take(&mut self.operations),
            coalesced: coalesced_windows,
            tombstones: self.tombstones,
            hot_objects: self.hot_objects.len() as u64,
            directories_touched: self.directories.len() as u64,
            recreates: self.recreates,
            extensions: sorted(&mut self.extensions),
            sizes: sorted(&mut self.sizes),
            depths: sorted(&mut self.depths),
            max_backlog: bucket_age(self.max_backlog_s),
        };
        let reasons = ReasonSummary {
            at: now,
            volume,
            interval_s,
            combinations,
            close_records: self.close_records,
            intermediate_records: self.intermediate_records,
            directory_records: self.directory_records,
        };
        let output = IntervalOutput {
            rate,
            reasons,
            backlog_ms: std::mem::replace(&mut self.backlog_ms, Histogram::new()),
            logical_changes: self.logical_changes,
            coalesced: self.coalesced,
        };
        self.interval_started_s = now_s;
        self.per_second.clear();
        self.records = 0;
        self.logical_changes = 0;
        self.coalesced = 0;
        self.close_records = 0;
        self.intermediate_records = 0;
        self.directory_records = 0;
        self.tombstones = 0;
        self.recreates = 0;
        self.hot_objects.clear();
        self.directories.clear();
        self.edit_counts.clear();
        self.fan_out.clear();
        self.max_backlog_s = 0;
        output
    }
}

fn sorted<K: Copy + Ord>(map: &mut HashMap<K, u64>) -> Vec<(K, u64)> {
    let mut items: Vec<(K, u64)> = map.drain().collect();
    items.sort();
    items
}

/// Operation implied by the reason bits of a close record. Delete wins over
/// everything (the object is gone), create over rename/update (the object
/// is new to us regardless of what happened to it since), and data changes
/// over metadata-only reasons.
pub fn classify(reason: u32) -> ChangeOperation {
    if reason & REASON_FILE_DELETE != 0 {
        ChangeOperation::Delete
    } else if reason & REASON_FILE_CREATE != 0 {
        ChangeOperation::Create
    } else if reason & REASON_RENAME_NEW_NAME != 0 {
        ChangeOperation::Rename
    } else if reason & DATA_MASK != 0 {
        ChangeOperation::Update
    } else if reason & REASON_HARD_LINK_CHANGE != 0 {
        ChangeOperation::HardLink
    } else if reason & STREAM_MASK != 0 {
        ChangeOperation::Stream
    } else {
        ChangeOperation::Metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidos_observe::AgeBucket;

    fn key() -> StudyKey {
        StudyKey::from_bytes([5; 32])
    }

    fn at(seconds: i64) -> TimeAnchor {
        TimeAnchor {
            monotonic_ns: seconds as u64 * 1_000_000_000,
            utc_ns: seconds * 1_000_000_000,
        }
    }

    fn record(
        usn: i64,
        frn: u128,
        parent: u128,
        reason: u32,
        name: &'static str,
    ) -> RecordView<'static> {
        RecordView {
            usn,
            frn,
            parent_frn: parent,
            reason,
            is_directory: false,
            name,
            timestamp_ns: 0,
        }
    }

    #[test]
    fn classification_priorities() {
        assert_eq!(
            classify(REASON_FILE_CREATE | REASON_DATA_EXTEND | REASON_FILE_DELETE | REASON_CLOSE),
            ChangeOperation::Delete
        );
        assert_eq!(
            classify(REASON_FILE_CREATE | REASON_RENAME_NEW_NAME | REASON_CLOSE),
            ChangeOperation::Create
        );
        assert_eq!(
            classify(REASON_RENAME_NEW_NAME | REASON_DATA_OVERWRITE | REASON_CLOSE),
            ChangeOperation::Rename
        );
        assert_eq!(
            classify(REASON_DATA_TRUNCATION | REASON_CLOSE),
            ChangeOperation::Update
        );
        assert_eq!(
            classify(REASON_HARD_LINK_CHANGE | REASON_CLOSE),
            ChangeOperation::HardLink
        );
        assert_eq!(
            classify(REASON_NAMED_DATA_EXTEND | REASON_CLOSE),
            ChangeOperation::Stream
        );
        assert_eq!(classify(0x8000 | REASON_CLOSE), ChangeOperation::Metadata);
    }

    #[test]
    fn intermediate_records_coalesce_into_one_change() {
        let key = key();
        let mut analyzer = ChangeAnalyzer::new(b"vol", 100);
        let facts = ObjectFacts {
            size: Some(5_000),
            depth: Some(4),
        };
        assert!(analyzer
            .observe(
                &key,
                &record(1, 7, 2, REASON_FILE_CREATE, "a.rs"),
                facts,
                at(100)
            )
            .is_none());
        assert!(analyzer
            .observe(
                &key,
                &record(2, 7, 2, REASON_FILE_CREATE | REASON_DATA_EXTEND, "a.rs"),
                facts,
                at(100)
            )
            .is_none());
        let change = analyzer
            .observe(
                &key,
                &record(
                    3,
                    7,
                    2,
                    REASON_FILE_CREATE | REASON_DATA_EXTEND | REASON_CLOSE,
                    "a.rs",
                ),
                facts,
                at(101),
            )
            .unwrap();
        assert_eq!(change.operation, ChangeOperation::Create);
        assert_eq!(change.extension, ExtensionBucket::Source);
        assert_eq!(change.size, SizeBucket::B16K);
        assert_eq!(change.depth, DepthBucket::Medium);
        assert_eq!(change.edit_count, CountBucket::One);
        assert_eq!(change.object, analyzer.object_token(&key, 7));
        let output = analyzer.flush(&key, at(160));
        assert_eq!(output.rate.records, 3);
        assert_eq!(output.rate.logical_changes, 1);
        assert_eq!(output.coalesced, 2);
        assert_eq!(output.reasons.close_records, 1);
        assert_eq!(output.reasons.intermediate_records, 2);
        assert_eq!(output.rate.operations.creates, 1);
        assert_eq!(output.rate.interval_s, 60);
        assert_eq!(output.rate.per_second.total, 60);
        assert_eq!(output.rate.per_second.counts[0], 59);
    }

    #[test]
    fn rename_pairs_the_old_parent_and_moves_show_a_different_subtree() {
        let key = key();
        let mut analyzer = ChangeAnalyzer::new(b"vol", 0);
        let facts = ObjectFacts::default();
        assert!(analyzer
            .observe(
                &key,
                &record(1, 9, 20, REASON_RENAME_OLD_NAME, "old.txt"),
                facts,
                at(1)
            )
            .is_none());
        let change = analyzer
            .observe(
                &key,
                &record(2, 9, 30, REASON_RENAME_NEW_NAME | REASON_CLOSE, "new.txt"),
                facts,
                at(1),
            )
            .unwrap();
        assert_eq!(change.operation, ChangeOperation::Rename);
        let old_parent = key.token("subtree", &analyzer.identity(20));
        assert_eq!(change.rename_pair, Some(old_parent));
        assert_ne!(change.rename_pair, Some(change.subtree.clone()));

        // An in-place rename pairs with the same subtree.
        analyzer.observe(
            &key,
            &record(3, 9, 30, REASON_RENAME_OLD_NAME, "new.txt"),
            facts,
            at(2),
        );
        let same = analyzer
            .observe(
                &key,
                &record(4, 9, 30, REASON_RENAME_NEW_NAME | REASON_CLOSE, "newer.txt"),
                facts,
                at(2),
            )
            .unwrap();
        assert_eq!(same.rename_pair, Some(same.subtree.clone()));
        assert_eq!(same.size, SizeBucket::Unknown);
    }

    #[test]
    fn delete_then_recreate_reports_the_interval_and_hot_edits_are_counted() {
        let key = key();
        let mut analyzer = ChangeAnalyzer::new(b"vol", 0);
        let facts = ObjectFacts::default();
        let deleted = analyzer
            .observe(
                &key,
                &record(1, 5, 2, REASON_FILE_DELETE | REASON_CLOSE, "Build.log"),
                facts,
                at(10),
            )
            .unwrap();
        assert_eq!(deleted.operation, ChangeOperation::Delete);
        assert_eq!(deleted.delete_recreate_age, None);
        // A new FRN, same parent and (case-folded) name, 5 minutes later.
        let recreated = analyzer
            .observe(
                &key,
                &record(2, 6, 2, REASON_FILE_CREATE | REASON_CLOSE, "build.LOG"),
                facts,
                at(310),
            )
            .unwrap();
        assert_eq!(recreated.delete_recreate_age, Some(AgeBucket::Minutes));
        assert_ne!(recreated.object, deleted.object);

        for usn in 0..12 {
            analyzer.observe(
                &key,
                &record(
                    100 + usn,
                    6,
                    2,
                    REASON_DATA_OVERWRITE | REASON_CLOSE,
                    "build.log",
                ),
                facts,
                at(320 + usn),
            );
        }
        let output = analyzer.flush(&key, at(400));
        assert_eq!(output.rate.hot_objects, 1);
        assert_eq!(output.rate.tombstones, 1);
        assert_eq!(output.rate.recreates, 1);
        assert_eq!(output.rate.operations.updates, 12);
        assert!(output.rate.extensions.contains(&(ExtensionBucket::Log, 14)));
    }

    #[test]
    fn coalescing_windows_count_rows_a_batcher_would_ship() {
        let key = key();
        let mut analyzer = ChangeAnalyzer::new(b"vol", 0);
        let facts = ObjectFacts::default();
        // Ten edits to one object, one per second, then one to another.
        for second in 0..10 {
            analyzer.observe(
                &key,
                &record(second, 1, 2, REASON_DATA_OVERWRITE | REASON_CLOSE, "x"),
                facts,
                at(second),
            );
        }
        analyzer.observe(
            &key,
            &record(20, 3, 2, REASON_DATA_OVERWRITE | REASON_CLOSE, "y"),
            facts,
            at(10),
        );
        let output = analyzer.flush(&key, at(60));
        assert_eq!(output.rate.logical_changes, 11);
        // Per-second batching ships every change; 60 s batching ships two rows.
        assert_eq!(output.rate.coalesced.w1s, 11);
        assert_eq!(output.rate.coalesced.w10s, 2);
        assert_eq!(output.rate.coalesced.w60s, 2);
        // Longer windows have not rolled yet, so they report nothing here...
        assert_eq!(output.rate.coalesced.w600s, 0);
        // ...and flush their rows in the interval where they roll.
        let later = analyzer.flush(&key, at(700));
        assert_eq!(later.rate.coalesced.w600s, 2);
        assert_eq!(later.rate.logical_changes, 0);
    }

    #[test]
    fn tokens_never_contain_names_and_are_volume_scoped() {
        let key = key();
        let a = ChangeAnalyzer::new(b"vol-a", 0);
        let b = ChangeAnalyzer::new(b"vol-b", 0);
        assert_ne!(a.object_token(&key, 1), b.object_token(&key, 1));
        assert_eq!(a.object_token(&key, 1), a.object_token(&key, 1));
        let json = serde_json::to_string(&a.object_token(&key, 1)).unwrap();
        assert_eq!(json.len(), 66);
    }
}
