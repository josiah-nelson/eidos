use crate::families::{
    AccessSummary, ContentObservation, EnumerationProbe, FeedHealthRecord, RateSummary,
    ReasonSummary, ResourceSample, VolumeObservation,
};
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
    /// Present only in bundles produced by the Windows collector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<WindowsCapabilities>,
}

/// Windows lanes are independent facts: the USN journal needs volume
/// management rights, the ETW session needs the Performance Log Users right
/// or SYSTEM, and the DPAPI study key must decrypt under the service account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsCapabilities {
    pub usn: UsnState,
    pub etw: EtwState,
    pub running_as_system: bool,
    pub elevated: bool,
    pub study_key_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsnState {
    Available,
    AccessDenied,
    NoJournaledVolume,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EtwState {
    Off,
    Available,
    AccessDenied,
    SessionConflict,
    InternalError,
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
    /// The USN journal was deleted and recreated (new journal id).
    JournalRecreated,
    /// The native feed could not be opened (rights, offline volume).
    FeedUnavailable,
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
    Volume(VolumeObservation),
    FeedHealth(FeedHealthRecord),
    Rate(RateSummary),
    Reasons(ReasonSummary),
    Access(AccessSummary),
    Content(ContentObservation),
    Enumeration(EnumerationProbe),
    Resource(ResourceSample),
}

impl ObservationRecord {
    /// Detailed records are per-object and live in the shorter ring;
    /// summaries are aggregates and keep the longer retention.
    pub fn is_detailed(&self) -> bool {
        matches!(
            self,
            Self::LogicalChange(_) | Self::Apfs(_) | Self::Content(_)
        )
    }

    pub fn at(&self) -> &TimeAnchor {
        match self {
            Self::Health(value) => &value.at,
            Self::LogicalChange(value) => &value.at,
            Self::Workload(value) => &value.at,
            Self::Apfs(value) => &value.at,
            Self::Mark(value) => &value.at,
            Self::Volume(value) => &value.at,
            Self::FeedHealth(value) => &value.at,
            Self::Rate(value) => &value.at,
            Self::Reasons(value) => &value.at,
            Self::Access(value) => &value.at,
            Self::Content(value) => &value.at,
            Self::Enumeration(value) => &value.at,
            Self::Resource(value) => &value.at,
        }
    }

    pub fn utc_ns(&self) -> i64 {
        self.at().utc_ns
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
    /// Stop requested by system shutdown rather than by an operator.
    Shutdown,
    /// AC/battery transition or low-battery notification.
    PowerStatusChange,
    /// The collector's build hash differs from the previous run's.
    Upgraded,
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
    /// Attribute, timestamp, security, or reparse-point change without a
    /// data change.
    Metadata,
    /// Hard link added or removed.
    HardLink,
    /// Named (alternate) stream change.
    Stream,
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
    /// Compilers, linkers, build drivers, package managers.
    Build,
    /// Search indexers, thumbnail/metadata scanners (including the eidos
    /// core service).
    Indexer,
    /// The observatory collector itself: its spool writes and ETW access
    /// are the cost of observing, not of the workload or of eidos.
    Collector,
    /// Backup and imaging agents.
    Backup,
    /// Cloud file-sync clients.
    CloudSync,
    /// Anti-malware and security agents.
    Security,
    /// Shell, explorer, terminals, launchers.
    Shell,
    /// Virtual machine hosts and container runtimes.
    Virtualization,
    /// Keyed token of the image file name when no class matched.
    ImageToken(ObjectToken),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepthBucket {
    Root,
    Shallow,
    Medium,
    Deep,
    VeryDeep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    /// Compiler and build-system intermediates.
    Build,
    /// Virtual disks, installation images, VM state.
    DiskImage,
    Log,
    Temporary,
    Other,
}
