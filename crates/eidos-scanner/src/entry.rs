//! Platform-neutral enumeration contracts.

use crate::error::ScanError;
use eidos_domain::{FileAttributes, NativeIdentity, ObjectKind, UnixNanos};
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
    /// Raw `FILE_*` filesystem flags from the OS.
    pub fs_flags: u32,
    pub drive_type: DriveType,
    pub supports_file_ids: bool,
    pub supports_usn: bool,
    pub supports_hard_links: bool,
    pub supports_reparse_points: bool,
    pub supports_sparse: bool,
    pub bytes_per_cluster: u32,
    /// Root of the volume containing the path (e.g. `G:\`), or the UNC share.
    pub volume_root: String,
}

impl VolumeInfo {
    /// Native NTFS/ReFS on a local disk: eligible for the MFT/USN fast path.
    pub fn is_native_local(&self) -> bool {
        matches!(self.drive_type, DriveType::Fixed | DriveType::Removable)
            && (self.filesystem.eq_ignore_ascii_case("NTFS")
                || self.filesystem.eq_ignore_ascii_case("ReFS"))
            && self.supports_usn
    }

    pub fn is_remote(&self) -> bool {
        self.drive_type == DriveType::Remote
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
