//! Record families added for the Windows collector.
//!
//! Aggregate counts in these records are exact: they describe how many
//! events happened, not which objects. Anything that names an object, a
//! volume, or a process image is a keyed token, and every per-object scalar
//! is bucketed.

use crate::privacy::ObjectToken;
use crate::schema::{
    AgeBucket, DepthBucket, ExtensionBucket, FeedCursor, FeedKind, ProcessClass, SizeBucket,
    TimeAnchor,
};
use serde::{Deserialize, Serialize};

/// Fixed log2 histogram: bucket `i` counts values in `[2^(i-1), 2^i)`, bucket
/// 0 counts zero. Twenty-four buckets cover values up to 8 Mi; larger values
/// land in the last bucket and `max` keeps the true maximum.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    pub counts: Vec<u64>,
    pub total: u64,
    pub max: u64,
}

impl Histogram {
    pub const BUCKETS: usize = 24;

    pub fn new() -> Self {
        Self {
            counts: vec![0; Self::BUCKETS],
            total: 0,
            max: 0,
        }
    }

    pub fn bucket_of(value: u64) -> usize {
        if value == 0 {
            0
        } else {
            ((64 - value.leading_zeros()) as usize).min(Self::BUCKETS - 1)
        }
    }

    pub fn observe(&mut self, value: u64) {
        if self.counts.len() != Self::BUCKETS {
            self.counts.resize(Self::BUCKETS, 0);
        }
        self.counts[Self::bucket_of(value)] += 1;
        self.total += 1;
        self.max = self.max.max(value);
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Upper bound of the bucket holding the `percent`-th percentile.
    pub fn percentile_bound(&self, percent: u8) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let target = ((self.total as u128 * percent as u128).div_ceil(100) as u64).max(1);
        let mut seen = 0;
        for (index, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= target {
                return if index == 0 {
                    0
                } else if index == Self::BUCKETS - 1 {
                    // The final bucket is open-ended; its true upper bound is
                    // the largest observation, not 2^(BUCKETS-1).
                    self.max
                } else {
                    1u64 << index
                };
            }
        }
        self.max
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityBucket {
    Unknown,
    LessThan1G,
    G4,
    G16,
    G64,
    G256,
    T1,
    T4,
    T16,
    Larger,
}

pub fn bucket_capacity(bytes: u64) -> CapacityBucket {
    const G: u64 = 1 << 30;
    match bytes {
        0 => CapacityBucket::Unknown,
        b if b < G => CapacityBucket::LessThan1G,
        b if b < 4 * G => CapacityBucket::G4,
        b if b < 16 * G => CapacityBucket::G16,
        b if b < 64 * G => CapacityBucket::G64,
        b if b < 256 * G => CapacityBucket::G256,
        b if b < 1024 * G => CapacityBucket::T1,
        b if b < 4096 * G => CapacityBucket::T4,
        b if b < 16384 * G => CapacityBucket::T16,
        _ => CapacityBucket::Larger,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PercentBucket {
    Zero,
    Under10,
    Under25,
    Under50,
    Under75,
    Under90,
    Under100,
    Full,
}

impl PercentBucket {
    pub fn from_ratio(part: u64, whole: u64) -> Self {
        if whole == 0 || part == 0 {
            return Self::Zero;
        }
        let percent = (part as u128 * 100 / whole as u128) as u64;
        match percent {
            0 => Self::Zero,
            1..=9 => Self::Under10,
            10..=24 => Self::Under25,
            25..=49 => Self::Under50,
            50..=74 => Self::Under75,
            75..=89 => Self::Under90,
            90..=99 => Self::Under100,
            _ => Self::Full,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeObservation {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub event: VolumeEvent,
    pub filesystem: FilesystemKind,
    pub drive: DriveKind,
    pub bus: BusKind,
    pub media: MediaKind,
    pub capacity: CapacityBucket,
    pub free: PercentBucket,
    pub bytes_per_cluster: u32,
    pub case_sensitive: Option<bool>,
    pub supports_usn: bool,
    pub supports_file_ids: bool,
    pub supports_sparse: bool,
    pub supports_reparse_points: bool,
    pub supports_hard_links: bool,
    pub compressed: bool,
    pub journal: Option<JournalShape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeEvent {
    Inventory,
    Mounted,
    Unmounted,
    JournalCreated,
    JournalRecreated,
    JournalDeleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemKind {
    Ntfs,
    Refs,
    Fat,
    Exfat,
    Udf,
    Apfs,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    Fixed,
    Removable,
    Remote,
    Optical,
    RamDisk,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusKind {
    Nvme,
    Sata,
    Sas,
    Scsi,
    Usb,
    Sd,
    /// Hypervisor-presented disk (a VM's virtual controller).
    Virtual,
    /// A mounted VHD/VHDX or ISO on the host.
    FileBacked,
    Other,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// No seek penalty reported.
    Solid,
    /// Seek penalty reported (rotational).
    Rotational,
    Unknown,
}

/// Shape of a USN journal without its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalShape {
    pub maximum_size: SizeBucket,
    pub allocation_delta: SizeBucket,
    /// `next_usn - first_usn`: how much history the journal currently holds.
    pub span: SizeBucket,
    pub max_major_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedHealthRecord {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub feed: FeedKind,
    pub state: FeedState,
    pub cursor: Option<FeedCursor>,
    /// Journal bytes between the collector's cursor and the journal head.
    pub lag: SizeBucket,
    /// How full the journal is relative to its maximum size.
    pub fill: PercentBucket,
    pub batches: u64,
    pub records: u64,
    pub logical_changes: u64,
    pub coalesced: u64,
    pub overflows: u64,
    pub recreations: u64,
    pub read_errors: u64,
    /// Time from a record's journal timestamp to the collector processing it.
    pub backlog_ms: Histogram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedState {
    Starting,
    Live,
    Overflowed,
    Recreated,
    AccessDenied,
    NotActive,
    Unsupported,
    Offline,
    Stopped,
}

/// Change workload and shadow-sync economics for one volume and interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateSummary {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub interval_s: u32,
    pub records: u64,
    pub logical_changes: u64,
    /// Logical changes per second across the interval's seconds.
    pub per_second: Histogram,
    pub operations: OperationCounts,
    /// Distinct objects touched when changes are coalesced at each window
    /// width. The ratio to `logical_changes` is the materialized-row saving.
    pub coalesced: CoalescingWindows,
    pub tombstones: u64,
    /// Objects edited ten or more times within the interval.
    pub hot_objects: u64,
    pub directories_touched: u64,
    /// Create events that recreated an object deleted within the interval.
    pub recreates: u64,
    pub extensions: Vec<(ExtensionBucket, u64)>,
    pub sizes: Vec<(SizeBucket, u64)>,
    pub depths: Vec<(DepthBucket, u64)>,
    pub max_backlog: AgeBucket,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationCounts {
    pub creates: u64,
    pub updates: u64,
    pub deletes: u64,
    pub renames: u64,
    pub metadata: u64,
    pub hard_links: u64,
    pub streams: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoalescingWindows {
    pub w1s: u64,
    pub w10s: u64,
    pub w60s: u64,
    pub w600s: u64,
    pub w3600s: u64,
}

/// Native reason-flag combinations seen on one volume in one interval. The
/// masks are USN reason bits; they carry no identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonSummary {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub interval_s: u32,
    pub combinations: Vec<(u32, u64)>,
    pub close_records: u64,
    pub intermediate_records: u64,
    pub directory_records: u64,
}

/// Access telemetry for one process class in one interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessSummary {
    pub at: TimeAnchor,
    pub interval_s: u32,
    pub process: ProcessClass,
    pub process_starts: u64,
    pub opens: u64,
    pub reads: u64,
    pub writes: u64,
    pub closes: u64,
    pub deletes: u64,
    pub renames: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub distinct_objects: u64,
    /// Objects both read and written by this class in the interval.
    pub read_write_objects: u64,
    pub read_size: Histogram,
    pub write_size: Histogram,
    pub extensions: Vec<(ExtensionBucket, u64)>,
}

/// Content economics for one sampled object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentObservation {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub object: ObjectToken,
    pub size: SizeBucket,
    pub extension: ExtensionBucket,
    pub outcome: ContentOutcome,
    /// Keyed fingerprint of the whole content; equal across hosts sharing a
    /// study key.
    pub fingerprint: Option<ObjectToken>,
    pub chunker: ChunkerKind,
    pub chunks: u32,
    pub chunk_size: Histogram,
    /// Chunks whose keyed hash matched the previous observation of this
    /// object; measures edit locality.
    pub reused_chunks: u32,
    /// Lengths of contiguous reused-chunk runs.
    pub reuse_runs: Histogram,
    /// Compressed size relative to logical size.
    pub compressed: PercentBucket,
    pub read_ms: u32,
    pub text_like: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentOutcome {
    Measured,
    SkippedPlaceholder,
    SkippedOffline,
    SkippedReparse,
    SkippedTooLarge,
    SkippedSharing,
    SkippedAccessDenied,
    Vanished,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkerKind {
    None,
    FastCdc { min: u32, average: u32, max: u32 },
    Fixed { size: u32 },
}

/// A read-only enumeration of one volume with the production lister.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumerationProbe {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub duration_ms: u64,
    pub cpu_ms: u64,
    pub files: u64,
    pub directories: u64,
    pub errors: u64,
    pub max_depth: DepthBucket,
    pub fan_out: Histogram,
    pub sizes: Vec<(SizeBucket, u64)>,
    pub extensions: Vec<(ExtensionBucket, u64)>,
    pub reparse_points: u64,
    pub placeholders: u64,
    pub sparse: u64,
    pub compressed: u64,
    pub encrypted: u64,
    pub offline: u64,
    pub hard_linked: u64,
    /// Objects whose allocated size is below their logical size.
    pub under_allocated: u64,
}

/// Collector and host resource sample; `lanes` records which lanes were on
/// so observer effect can be compared across windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSample {
    pub at: TimeAnchor,
    pub interval_s: u32,
    pub collector: ProcessResources,
    pub system: HostResources,
    pub lanes: LaneStates,
}

/// The collector's own footprint in exact bytes (it is not user data).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessResources {
    pub cpu_ms: u64,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub handles: u32,
    pub threads: u32,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostResources {
    pub cpu_busy_percent: u8,
    pub memory_used_percent: u8,
    pub memory_total: CapacityBucket,
    pub logical_processors: u32,
    pub uptime: AgeBucket,
    pub on_battery: Option<bool>,
    /// Cumulative time the machine spent asleep since boot.
    pub slept_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneStates {
    pub usn: bool,
    pub etw: bool,
    pub content_probe: bool,
    pub enumeration_probe: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_log2_and_percentiles_are_upper_bounds() {
        let mut histogram = Histogram::new();
        for value in [0, 1, 2, 3, 4, 7, 8, 1_000, 1 << 40] {
            histogram.observe(value);
        }
        assert_eq!(histogram.total, 9);
        assert_eq!(histogram.max, 1 << 40);
        assert_eq!(histogram.counts[0], 1);
        assert_eq!(histogram.counts[1], 1);
        assert_eq!(histogram.counts[2], 2);
        assert_eq!(histogram.counts[3], 2);
        assert_eq!(histogram.counts[Histogram::BUCKETS - 1], 1);
        assert_eq!(histogram.percentile_bound(50), 8);
        // The top bucket is open-ended, so its bound is the observed max
        // rather than the 2^23 bucket floor.
        assert_eq!(histogram.percentile_bound(100), 1 << 40);
        assert_eq!(Histogram::default().percentile_bound(99), 0);
        let mut lazy = Histogram::default();
        lazy.observe(5);
        assert_eq!(lazy.counts.len(), Histogram::BUCKETS);
    }

    #[test]
    fn capacity_and_percent_buckets() {
        assert_eq!(bucket_capacity(0), CapacityBucket::Unknown);
        assert_eq!(bucket_capacity(500 << 30), CapacityBucket::T1);
        assert_eq!(bucket_capacity(3 << 40), CapacityBucket::T4);
        assert_eq!(PercentBucket::from_ratio(0, 10), PercentBucket::Zero);
        assert_eq!(PercentBucket::from_ratio(5, 10), PercentBucket::Under75);
        assert_eq!(PercentBucket::from_ratio(10, 10), PercentBucket::Full);
        assert_eq!(PercentBucket::from_ratio(3, 0), PercentBucket::Zero);
    }
}
