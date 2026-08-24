//! Defensive, read-only access to VHDX virtual disks.
//!
//! Container-declared sizes and counts are checked before allocation. The
//! virtual disk is exposed as a `Read + Seek` stream without booting or
//! mounting the image. Differencing images are identified and their parent
//! locator is reported, but their payload is not exposed without the parent.

#[cfg(test)]
mod fixture;
pub mod partition;
pub mod vhdx;
pub mod volume;

use eidos_domain::UnixNanos;
use serde::{Deserialize, Serialize};

pub use partition::{Partition, PartitionScheme};
pub use vhdx::{ParentLocator, PayloadKind, VhdxDisk, VhdxInfo};
pub use volume::VolumeReport;

/// How far an inventory got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    /// A budget, damaged record, or unsupported linked structure made the
    /// result incomplete; every emitted member is still valid.
    Partial,
    /// A differencing disk cannot be interpreted without its parent image.
    NeedsParent,
}

/// Resource budgets applied while opening one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskImageLimits {
    /// Live, named MFT records retained across every volume in one image.
    /// NTFS metafiles count toward this aggregate budget.
    pub max_members: u64,
    /// MFT records read per volume before the scan is cut short.
    pub max_mft_records: u64,
    /// Longest virtual path accepted for one member, in UTF-8 bytes.
    pub max_path_bytes: usize,
    /// Most path segments accepted for one member.
    pub max_path_depth: usize,
    /// Partition-table entries examined.
    pub max_partitions: usize,
    /// GPT partition-array bytes checksummed before entries are trusted.
    pub max_partition_table_bytes: u64,
    /// Region-table entries accepted (the VHDX specification's own cap).
    pub max_region_entries: u32,
    /// Metadata-table entries accepted (likewise).
    pub max_metadata_entries: u16,
    /// Largest virtual disk size accepted, in bytes.
    pub max_virtual_size: u64,
    /// Block-allocation-table entries loaded, capping BAT memory at 8 bytes
    /// each.
    pub max_bat_entries: u64,
    /// Bytes retained for one parent-locator key or value.
    pub max_locator_bytes: usize,
}

impl Default for DiskImageLimits {
    fn default() -> Self {
        Self {
            max_members: 1_000_000,
            max_mft_records: 4_000_000,
            max_path_bytes: 4096,
            max_path_depth: 256,
            max_partitions: 128,
            max_partition_table_bytes: 16 * 1024 * 1024,
            max_region_entries: 2047,
            max_metadata_entries: 2047,
            max_virtual_size: 64 << 40,
            max_bat_entries: 4 << 20,
            max_locator_bytes: 4096,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiskImageError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// No recognisable container signature: not an image this crate can read.
    #[error("not a VHDX image")]
    NotDiskImage,
    /// Structure inconsistent with the VHDX specification.
    #[error("corrupt image: {0}")]
    Corrupt(String),
    /// Well-formed but beyond what this reader supports.
    #[error("unsupported image: {0}")]
    Unsupported(String),
    /// The `ntfs` crate rejected the filesystem.
    #[error("unreadable NTFS volume: {0}")]
    Ntfs(String),
}

/// Why a member's stored name was not taken at face value (bit flags).
pub mod flag {
    pub const TRAVERSAL: u32 = 1 << 0;
    pub const SEPARATOR: u32 = 1 << 1;
    pub const CONTROL: u32 = 1 << 2;
    pub const ENCODING: u32 = 1 << 3;
    pub const EMPTY: u32 = 1 << 4;
    pub const ORPHAN: u32 = 1 << 5;
    pub const SHORT_NAME: u32 = 1 << 6;
}

/// One live directory entry found inside an image. Hard-linked files produce
/// one member per long/POSIX `$FILE_NAME`, sharing the same MFT `record`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub volume: u32,
    pub record: u64,
    pub parent_record: u64,
    /// Normalised virtual path relative to the volume root.
    pub path: String,
    pub name: String,
    pub parent: String,
    pub is_dir: bool,
    pub size: u64,
    pub allocated: u64,
    /// Whether `allocated` was computed from concrete NTFS data runs. When
    /// false it is a conservative cluster-rounded upper bound.
    pub allocation_exact: bool,
    pub created: Option<UnixNanos>,
    pub modified: Option<UnixNanos>,
    pub accessed: Option<UnixNanos>,
    pub changed: Option<UnixNanos>,
    pub hard_links: u16,
    /// `flag::*` bits, including flags inherited from path ancestors.
    pub flags: u32,
}
