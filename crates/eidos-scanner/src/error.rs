//! Scan error classification.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanErrorKind {
    AccessDenied,
    NotFound,
    /// Sharing violation, network hiccup, device not ready: worth retrying.
    Transient,
    /// The lister does not support this path/volume (e.g. info class unsupported).
    Unsupported,
    /// Name could not be represented losslessly.
    InvalidName,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?} at {path:?}: {message} (os {code})")]
pub struct ScanError {
    pub kind: ScanErrorKind,
    pub code: i32,
    pub message: String,
    pub path: PathBuf,
}

impl ScanError {
    pub fn from_io(path: &std::path::Path, e: &std::io::Error) -> Self {
        let code = e.raw_os_error().unwrap_or(0);
        Self {
            kind: classify_os_error(code, e.kind()),
            code,
            message: e.to_string(),
            path: path.to_path_buf(),
        }
    }

    pub fn new(
        kind: ScanErrorKind,
        code: i32,
        message: impl Into<String>,
        path: &std::path::Path,
    ) -> Self {
        Self {
            kind,
            code,
            message: message.into(),
            path: path.to_path_buf(),
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.kind == ScanErrorKind::Transient
    }
}

/// Map a Win32 error code (or io::ErrorKind elsewhere) to a scan error kind.
pub fn classify_os_error(code: i32, kind: std::io::ErrorKind) -> ScanErrorKind {
    use std::io::ErrorKind as K;
    match code {
        5 | 1920 | 1314 => ScanErrorKind::AccessDenied, // ACCESS_DENIED, CANT_ACCESS_FILE, PRIVILEGE_NOT_HELD
        2 | 3 | 123 | 267 => ScanErrorKind::NotFound, // FILE/PATH_NOT_FOUND, INVALID_NAME, DIRECTORY
        32 | 33 | 21 | 53 | 64 | 59 | 1231 | 1232 | 121 | 1450 | 1453 | 170 => {
            ScanErrorKind::Transient
        } // SHARING_VIOLATION, LOCK_VIOLATION, NOT_READY, BAD_NETPATH, NETNAME_DELETED, UNEXP_NET_ERR, NETWORK_UNREACHABLE, HOST_UNREACHABLE, SEM_TIMEOUT, NO_SYSTEM_RESOURCES, WORKING_SET_QUOTA, BUSY
        50 | 124 | 87 | 1 => ScanErrorKind::Unsupported, // NOT_SUPPORTED, INVALID_LEVEL, INVALID_PARAMETER, INVALID_FUNCTION
        _ => match kind {
            K::PermissionDenied => ScanErrorKind::AccessDenied,
            K::NotFound => ScanErrorKind::NotFound,
            K::Interrupted | K::TimedOut | K::WouldBlock | K::ConnectionReset => {
                ScanErrorKind::Transient
            }
            K::Unsupported => ScanErrorKind::Unsupported,
            K::InvalidData | K::InvalidInput => ScanErrorKind::InvalidName,
            _ => ScanErrorKind::Other,
        },
    }
}
