//! Defensive, read-only access to VHDX virtual disks.
//!
//! Container-declared sizes and counts are checked before allocation. The
//! virtual disk is exposed as a `Read + Seek` stream without booting or
//! mounting the image. Differencing images are identified and their parent
//! locator is reported, but their payload is not exposed without the parent.

#[cfg(test)]
mod fixture;
pub mod vhdx;

use serde::{Deserialize, Serialize};

pub use vhdx::{ParentLocator, PayloadKind, VhdxDisk, VhdxInfo};

/// Resource budgets applied while opening one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskImageLimits {
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
}
