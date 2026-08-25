//! Portable lister built on `std::fs`. Used on non-Windows platforms and as a
//! reference implementation in tests. Provides no allocation size and only a
//! path-derived identity.

use crate::entry::{DirectoryLister, DriveType, RawEntry, VolumeInfo};
use crate::error::ScanError;
use eidos_domain::{FileAttributes, ObjectKind, UnixNanos};
use std::path::Path;

#[cfg(unix)]
fn native_identity(metadata: &std::fs::Metadata) -> Option<eidos_domain::NativeIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(eidos_domain::NativeIdentity {
        volume_serial: metadata.dev(),
        file_id_high: 0,
        file_id_low: metadata.ino(),
        // Unix inode reuse and volume remounts prevent a cross-run stability
        // claim, but the identity is useful for one reconciliation.
        confidence: eidos_domain::IdentityConfidence::Weak,
    })
}

#[cfg(not(unix))]
fn native_identity(_: &std::fs::Metadata) -> Option<eidos_domain::NativeIdentity> {
    None
}

#[cfg(windows)]
fn metadata_attributes(metadata: &std::fs::Metadata) -> FileAttributes {
    use std::os::windows::fs::MetadataExt;
    FileAttributes(metadata.file_attributes())
}

#[cfg(not(windows))]
fn metadata_attributes(metadata: &std::fs::Metadata) -> FileAttributes {
    let mut attributes = FileAttributes::default();
    if metadata.permissions().readonly() {
        attributes.0 |= FileAttributes::READONLY;
    }
    attributes
}

pub struct StdLister;

impl DirectoryLister for StdLister {
    fn list(&self, dir: &Path) -> Result<Vec<RawEntry>, ScanError> {
        let rd = std::fs::read_dir(dir).map_err(|e| ScanError::from_io(dir, &e))?;
        let mut out = Vec::new();
        for item in rd {
            let item = item.map_err(|e| ScanError::from_io(dir, &e))?;
            let name_os = item.file_name();
            let (name, name_lossy) = match name_os.to_str() {
                Some(s) => (s.to_string(), false),
                None => (name_os.to_string_lossy().into_owned(), true),
            };
            let md = match std::fs::symlink_metadata(item.path()) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!(path = %item.path().display(), error = %e, "metadata failed");
                    continue;
                }
            };
            let ft = md.file_type();
            let mut attributes = metadata_attributes(&md);
            let kind = if ft.is_symlink() {
                attributes.0 |= FileAttributes::REPARSE_POINT;
                ObjectKind::Reparse
            } else if ft.is_dir() {
                attributes.0 |= FileAttributes::DIRECTORY;
                ObjectKind::Directory
            } else {
                ObjectKind::File
            };
            let size = if kind == ObjectKind::File {
                md.len()
            } else {
                0
            };
            out.push(RawEntry {
                name,
                name_lossy,
                kind,
                attributes,
                size,
                allocated: None,
                created: md.created().ok().map(UnixNanos::from_system_time),
                modified: md.modified().ok().map(UnixNanos::from_system_time),
                changed: None,
                accessed: md.accessed().ok().map(UnixNanos::from_system_time),
                native_id: native_identity(&md),
                reparse_tag: 0,
            });
        }
        Ok(out)
    }

    fn volume_info(&self, root: &Path) -> Result<VolumeInfo, ScanError> {
        Ok(VolumeInfo {
            drive_type: DriveType::Unknown,
            volume_root: root.display().to_string(),
            ..Default::default()
        })
    }

    fn stat(&self, path: &Path) -> Result<RawEntry, ScanError> {
        let md = std::fs::symlink_metadata(path).map_err(|e| ScanError::from_io(path, &e))?;
        let ft = md.file_type();
        let mut attributes = metadata_attributes(&md);
        let kind = if ft.is_symlink() {
            attributes.0 |= FileAttributes::REPARSE_POINT;
            ObjectKind::Reparse
        } else if ft.is_dir() {
            attributes.0 |= FileAttributes::DIRECTORY;
            ObjectKind::Directory
        } else {
            ObjectKind::File
        };
        Ok(RawEntry {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            name_lossy: false,
            kind,
            attributes,
            size: if kind == ObjectKind::File {
                md.len()
            } else {
                0
            },
            allocated: None,
            created: md.created().ok().map(UnixNanos::from_system_time),
            modified: md.modified().ok().map(UnixNanos::from_system_time),
            changed: None,
            accessed: md.accessed().ok().map(UnixNanos::from_system_time),
            native_id: native_identity(&md),
            reparse_tag: 0,
        })
    }

    fn name(&self) -> &'static str {
        "std"
    }
}
