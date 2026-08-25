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

/// Kind and attributes of one object, decided the way this platform's native
/// adapter decides them. Windows classifies from the attribute bits, because
/// only a *directory* reparse point is a `Reparse` object there — a file
/// symlink or a cloud-backed file is a file with real bytes, and content
/// policy judges it by reparse tag. Unix classifies from the file type,
/// because a symlink there has no data of its own.
#[cfg(windows)]
fn classify(metadata: &std::fs::Metadata, _name: &str) -> (ObjectKind, FileAttributes) {
    use std::os::windows::fs::MetadataExt;
    let attributes = FileAttributes(metadata.file_attributes());
    (crate::win::object_kind(attributes), attributes)
}

/// macOS keeps hidden, compressed, immutable, and cloud-placeholder state in
/// BSD flags, so the fallback path decodes them exactly the way the native
/// lister does.
#[cfg(target_os = "macos")]
fn classify(metadata: &std::fs::Metadata, name: &str) -> (ObjectKind, FileAttributes) {
    use std::os::unix::fs::MetadataExt;
    let kind = unix_kind(metadata.file_type());
    let flags = std::os::macos::fs::MetadataExt::st_flags(metadata);
    (
        kind,
        crate::mac::attributes_from(kind, name, Some(metadata.mode()), Some(flags)),
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn classify(metadata: &std::fs::Metadata, name: &str) -> (ObjectKind, FileAttributes) {
    let kind = unix_kind(metadata.file_type());
    let mut attributes = FileAttributes::default();
    match kind {
        ObjectKind::Directory => attributes.0 |= FileAttributes::DIRECTORY,
        ObjectKind::Reparse => attributes.0 |= FileAttributes::REPARSE_POINT,
        _ => {}
    }
    if metadata.permissions().readonly() {
        attributes.0 |= FileAttributes::READONLY;
    }
    // Leading-dot names are hidden by convention on every Unix desktop.
    if name.starts_with('.') {
        attributes.0 |= FileAttributes::HIDDEN;
    }
    (kind, attributes)
}

#[cfg(not(any(windows, unix)))]
fn classify(metadata: &std::fs::Metadata, _name: &str) -> (ObjectKind, FileAttributes) {
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        ObjectKind::Directory
    } else {
        ObjectKind::File
    };
    let mut attributes = FileAttributes::default();
    if kind == ObjectKind::Directory {
        attributes.0 |= FileAttributes::DIRECTORY;
    }
    if metadata.permissions().readonly() {
        attributes.0 |= FileAttributes::READONLY;
    }
    (kind, attributes)
}

#[cfg(unix)]
fn unix_kind(file_type: std::fs::FileType) -> ObjectKind {
    if file_type.is_symlink() {
        ObjectKind::Reparse
    } else if file_type.is_dir() {
        ObjectKind::Directory
    } else {
        ObjectKind::File
    }
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
            let (kind, attributes) = classify(&md, &name);
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
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (kind, attributes) = classify(&md, &name);
        Ok(RawEntry {
            name,
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
