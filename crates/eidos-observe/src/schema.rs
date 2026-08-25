use crate::privacy::ObjectToken;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "eidos-observation/1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedCursor {
    pub feed: FeedKind,
    pub version: u16,
    /// Adapter-owned cursor bytes encoded without exposing native fields.
    pub opaque: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    Fsevents,
    Usn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSecurityState {
    Off,
    Available,
    NotEntitled,
    NotPermitted,
    NotPrivileged,
    TooManyClients,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSecurityCapability {
    pub state: EndpointSecurityState,
    pub entitlement_claimed: bool,
    pub tcc_full_disk_access: Option<bool>,
    pub running_as_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub fsevents: bool,
    pub endpoint_security: EndpointSecurityCapability,
    pub apfs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeAnchor {
    pub monotonic_ns: u64,
    pub utc_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureGap {
    pub started_monotonic_ns: u64,
    pub ended_monotonic_ns: u64,
    pub cause: GapCause,
    pub estimated_events: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapCause {
    FeedOverflow,
    KernelDrop,
    UserDrop,
    RootChanged,
    ClockJump,
    UncleanShutdown,
    CollectorStopped,
    KeyUnavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCounters {
    pub kernel: u64,
    pub user: u64,
    pub coalesced: u64,
    pub overflows: u64,
    pub root_changes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    pub schema: String,
    pub build_hash: String,
    pub config_hash: String,
    pub created: TimeAnchor,
    pub capabilities: Capabilities,
    pub capture_gaps: Vec<CaptureGap>,
    pub drops: DropCounters,
    pub units: Units,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Units {
    pub durations: String,
    pub sizes: String,
    pub counts: String,
}

impl Default for Units {
    fn default() -> Self {
        Self {
            durations: "bucket".into(),
            sizes: "bucket".into(),
            counts: "events".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationBundle {
    pub manifest: BundleManifest,
    pub records: Vec<ObservationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationRecord {
    Health(HealthRecord),
    LogicalChange(LogicalChange),
    Workload(WorkloadSummary),
    Apfs(ApfsObservation),
    Mark(MarkRecord),
}

impl ObservationRecord {
    pub fn is_detailed(&self) -> bool {
        matches!(self, Self::LogicalChange(_) | Self::Apfs(_))
    }

    pub fn utc_ns(&self) -> i64 {
        match self {
            Self::Health(value) => value.at.utc_ns,
            Self::LogicalChange(value) => value.at.utc_ns,
            Self::Workload(value) => value.at.utc_ns,
            Self::Apfs(value) => value.at.utc_ns,
            Self::Mark(value) => value.at.utc_ns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRecord {
    pub at: TimeAnchor,
    pub os_build: String,
    pub machine: MachineKind,
    pub lifecycle: LifecycleEvent,
    pub clean_prior_shutdown: Option<bool>,
    pub feed_cursor: Option<FeedCursor>,
    pub drops: DropCounters,
    pub cpu_millis: u64,
    pub resident_bytes_bucket: SizeBucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Physical,
    Virtual,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    Started,
    Heartbeat,
    Sleep,
    Wake,
    Restarted,
    ClockJump,
    Mounted,
    Unmounted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalChange {
    pub at: TimeAnchor,
    pub object: ObjectToken,
    pub subtree: ObjectToken,
    pub operation: ChangeOperation,
    pub rename_pair: Option<ObjectToken>,
    pub size: SizeBucket,
    pub extension: ExtensionBucket,
    pub depth: DepthBucket,
    pub edit_count: CountBucket,
    pub delete_recreate_age: Option<AgeBucket>,
    pub fan_out: CountBucket,
    pub backlog_age: AgeBucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOperation {
    Create,
    Update,
    Delete,
    Rename,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadSummary {
    pub at: TimeAnchor,
    pub process: ProcessClass,
    pub opens: CountBucket,
    pub closes: CountBucket,
    pub mappings: CountBucket,
    pub executions: CountBucket,
    pub changed_objects: CountBucket,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessClass {
    System,
    Development,
    Productivity,
    Media,
    Browser,
    Other,
    SigningToken(ObjectToken),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApfsObservation {
    pub at: TimeAnchor,
    pub volume: ObjectToken,
    pub object: ObjectToken,
    #[serde(rename = "apfs_kind")]
    pub kind: ApfsKind,
    pub prevalence: CountBucket,
    pub size: SizeBucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApfsKind {
    NativeIdentity,
    Clone,
    Sparse,
    Package,
    ExtendedAttribute,
    ResourceFork,
    Snapshot,
    ExternalVolume,
    CloudPlaceholder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRecord {
    pub at: TimeAnchor,
    /// Keyed token of the caller-selected marker; the label is never durable.
    pub marker: ObjectToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeBucket {
    Unknown,
    Zero,
    B1K,
    B4K,
    B16K,
    B64K,
    B256K,
    B1M,
    B4M,
    B16M,
    B64M,
    B256M,
    B1G,
    Larger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgeBucket {
    Immediate,
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Older,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepthBucket {
    Root,
    Shallow,
    Medium,
    Deep,
    VeryDeep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountBucket {
    Zero,
    One,
    Few,
    Tens,
    Hundreds,
    Thousands,
    Many,
}

impl From<u64> for CountBucket {
    fn from(value: u64) -> Self {
        match value {
            0 => Self::Zero,
            1 => Self::One,
            2..=9 => Self::Few,
            10..=99 => Self::Tens,
            100..=999 => Self::Hundreds,
            1_000..=9_999 => Self::Thousands,
            _ => Self::Many,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionBucket {
    None,
    Document,
    Source,
    Archive,
    Image,
    AudioVideo,
    Executable,
    Database,
    Configuration,
    Other,
}
