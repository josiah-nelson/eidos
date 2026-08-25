//! Platform-neutral enumeration contracts.

use crate::error::ScanError;
use eidos_domain::{FileAttributes, NativeIdentity, ObjectKind, SourceKind, UnixNanos};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One child of a directory as observed by a lister.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEntry {
    /// Exact display name. Lossy-decoded if the native name was not valid
    /// UTF-16; `name_lossy` records that.
    pub name: String,
    pub name_lossy: bool,
    pub kind: ObjectKind,
    pub attributes: FileAttributes,
    /// Logical (end-of-file) size. Zero for directories.
    pub size: u64,
    /// Allocated size on disk when the lister provides it; otherwise equal to
    /// `size` rounded up by the caller if desired. `None` means unknown.
    pub allocated: Option<u64>,
    pub created: Option<UnixNanos>,
    pub modified: Option<UnixNanos>,
    pub changed: Option<UnixNanos>,
    pub accessed: Option<UnixNanos>,
    pub native_id: Option<NativeIdentity>,
    /// Reparse tag when `attributes` has the reparse bit.
    pub reparse_tag: u32,
}

impl RawEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == ObjectKind::Directory
    }

    /// A directory that should be traversed by the walker. Reparse points are
    /// never traversed automatically.
    pub fn is_traversable_dir(&self) -> bool {
        self.kind == ObjectKind::Directory && !self.attributes.is_reparse()
    }
}

/// Volume capabilities detected for a root path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeInfo {
    pub volume_serial: u64,
    pub filesystem: String,
    pub volume_name: String,
    /// Raw filesystem flags from the OS: `FILE_*` on Windows, `MNT_*` mount
    /// flags on macOS. The value is platform-specific and is kept only for
    /// diagnostics; portable facts have their own fields.
    pub fs_flags: u32,
    pub drive_type: DriveType,
    pub supports_file_ids: bool,
    pub supports_usn: bool,
    pub supports_hard_links: bool,
    pub supports_reparse_points: bool,
    pub supports_sparse: bool,
    /// Whether names that differ only in case are distinct on this volume.
    /// `None` when the platform does not report it. NTFS reports
    /// case-preserving/insensitive behaviour; APFS volumes exist in both
    /// forms, so path equality must not be assumed either way.
    pub case_sensitive: Option<bool>,
    /// Native change feed this volume can drive, if any.
    pub native_feed: NativeFeed,
    pub bytes_per_cluster: u32,
    /// Root of the volume containing the path (e.g. `G:\`), the UNC share, or
    /// the mount point on macOS.
    pub volume_root: String,
}

impl VolumeInfo {
    /// A local volume with a native change feed: eligible for the incremental
    /// fast path instead of periodic reconciliation.
    pub fn is_native_local(&self) -> bool {
        self.native_feed != NativeFeed::None && !self.is_remote()
    }

    pub fn is_remote(&self) -> bool {
        self.drive_type == DriveType::Remote
    }

    /// The source adapter kind these capabilities imply on this host. Source
    /// kind is a property of the agent that owns the path, so the generic
    /// case is selected by the build target rather than by the filesystem.
    pub fn source_kind(&self) -> SourceKind {
        if self.is_remote() {
            return SourceKind::Smb;
        }
        match self.native_feed {
            NativeFeed::WindowsUsn => SourceKind::WindowsLocal,
            NativeFeed::MacosFsEvents => SourceKind::MacosLocal,
            NativeFeed::None => GENERIC_SOURCE_KIND,
        }
    }
}

/// Generic (crawl-only) source kind for the agent this build targets, used
/// both by [`VolumeInfo::source_kind`] and by callers that could not probe the
/// volume at all. Windows and macOS are the supported agents; any other target
/// reports the Windows generic kind so the catalog still round-trips.
#[cfg(target_os = "macos")]
pub const GENERIC_SOURCE_KIND: SourceKind = SourceKind::MacosGeneric;
#[cfg(not(target_os = "macos"))]
pub const GENERIC_SOURCE_KIND: SourceKind = SourceKind::WindowsGeneric;

/// Native change feed a volume can drive. Feeds do not share cursor
/// semantics: the USN journal is a durable byte-offset log, while FSEvents is
/// a coalescing event-id stream. The adapter owns those differences; this
/// enum only records which one exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativeFeed {
    /// No native feed: the source is kept fresh by reconciliation scans.
    #[default]
    None,
    /// NTFS/ReFS USN change journal.
    WindowsUsn,
    /// macOS FSEvents stream for a locally mounted volume.
    MacosFsEvents,
}

impl NativeFeed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WindowsUsn => "windows_usn",
            Self::MacosFsEvents => "macos_fsevents",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "windows_usn" => Self::WindowsUsn,
            "macos_fsevents" => Self::MacosFsEvents,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DriveType {
    #[default]
    Unknown,
    NoRootDir,
    Removable,
    Fixed,
    Remote,
    CdRom,
    RamDisk,
}

/// Anything that can list one directory's children.
pub trait DirectoryLister: Send + Sync {
    /// List the direct children of `dir`. The listing excludes `.` and `..`.
    fn list(&self, dir: &Path) -> Result<Vec<RawEntry>, ScanError>;

    /// Detect volume capabilities for `root`.
    fn volume_info(&self, root: &Path) -> Result<VolumeInfo, ScanError>;

    /// Stat a single path (used for the source root's own identity).
    fn stat(&self, path: &Path) -> Result<RawEntry, ScanError>;

    /// Human-readable adapter name for diagnostics.
    fn name(&self) -> &'static str;
}
