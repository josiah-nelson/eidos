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

/// Map a raw OS error code to a scan error kind, falling back to the portable
/// [`std::io::ErrorKind`]. Raw codes are only meaningful for the platform that
/// produced them: Win32 codes and POSIX `errno` values overlap numerically
/// (Win32 5 is `ACCESS_DENIED`, `errno` 5 is `EIO`), so each table is gated on
/// its own target rather than consulted everywhere.
pub fn classify_os_error(code: i32, kind: std::io::ErrorKind) -> ScanErrorKind {
    if let Some(classified) = classify_native(code) {
        return classified;
    }
    classify_io_kind(kind)
}

#[cfg(windows)]
fn classify_native(code: i32) -> Option<ScanErrorKind> {
    Some(match code {
        5 | 1920 | 1314 => ScanErrorKind::AccessDenied, // ACCESS_DENIED, CANT_ACCESS_FILE, PRIVILEGE_NOT_HELD
        2 | 3 | 123 | 267 => ScanErrorKind::NotFound, // FILE/PATH_NOT_FOUND, INVALID_NAME, DIRECTORY
        32 | 33 | 21 | 53 | 64 | 59 | 1231 | 1232 | 121 | 1450 | 1453 | 170 => {
            ScanErrorKind::Transient
        } // SHARING_VIOLATION, LOCK_VIOLATION, NOT_READY, BAD_NETPATH, NETNAME_DELETED, UNEXP_NET_ERR, NETWORK_UNREACHABLE, HOST_UNREACHABLE, SEM_TIMEOUT, NO_SYSTEM_RESOURCES, WORKING_SET_QUOTA, BUSY
        50 | 124 | 87 | 1 => ScanErrorKind::Unsupported, // NOT_SUPPORTED, INVALID_LEVEL, INVALID_PARAMETER, INVALID_FUNCTION
        _ => return None,
    })
}

/// `errno` values differ between Unix flavours, so the table is written in
/// terms of the platform's own constants rather than literals.
#[cfg(unix)]
fn classify_native(code: i32) -> Option<ScanErrorKind> {
    Some(match code {
        libc::EACCES | libc::EPERM => ScanErrorKind::AccessDenied,
        libc::ENOENT | libc::ENOTDIR | libc::ENAMETOOLONG => ScanErrorKind::NotFound,
        libc::EAGAIN
        | libc::EINTR
        | libc::EBUSY
        | libc::ETIMEDOUT
        | libc::ECONNRESET
        | libc::ECONNABORTED
        | libc::ENETDOWN
        | libc::ENETUNREACH
        | libc::EHOSTDOWN
        | libc::EHOSTUNREACH
        | libc::ESTALE
        | libc::ENFILE
        | libc::EMFILE
        | libc::EIO => ScanErrorKind::Transient,
        libc::ENOTSUP | libc::EINVAL | libc::ENOSYS | libc::ERANGE => ScanErrorKind::Unsupported,
        libc::EILSEQ => ScanErrorKind::InvalidName,
        _ => return None,
    })
}

#[cfg(not(any(windows, unix)))]
fn classify_native(_: i32) -> Option<ScanErrorKind> {
    None
}

fn classify_io_kind(kind: std::io::ErrorKind) -> ScanErrorKind {
    use std::io::ErrorKind as K;
    match kind {
        K::PermissionDenied => ScanErrorKind::AccessDenied,
        K::NotFound => ScanErrorKind::NotFound,
        K::Interrupted | K::TimedOut | K::WouldBlock | K::ConnectionReset => {
            ScanErrorKind::Transient
        }
        K::Unsupported => ScanErrorKind::Unsupported,
        K::InvalidData | K::InvalidInput => ScanErrorKind::InvalidName,
        _ => ScanErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two raw-code tables overlap numerically, so the wrong one silently
    /// mislabels ordinary failures: Win32 5 is `ACCESS_DENIED` while `errno` 5
    /// is `EIO`, and a retryable device error reported as access denied would
    /// never be retried.
    #[test]
    #[cfg(unix)]
    fn posix_codes_are_classified_as_posix() {
        assert_eq!(
            classify_os_error(libc::EIO, std::io::ErrorKind::Other),
            ScanErrorKind::Transient
        );
        assert_eq!(
            classify_os_error(libc::EACCES, std::io::ErrorKind::PermissionDenied),
            ScanErrorKind::AccessDenied
        );
        assert_eq!(
            classify_os_error(libc::ENOENT, std::io::ErrorKind::NotFound),
            ScanErrorKind::NotFound
        );
        assert_eq!(
            classify_os_error(libc::ENOTSUP, std::io::ErrorKind::Other),
            ScanErrorKind::Unsupported
        );
    }

    #[test]
    #[cfg(windows)]
    fn win32_codes_are_classified_as_win32() {
        assert_eq!(
            classify_os_error(5, std::io::ErrorKind::PermissionDenied),
            ScanErrorKind::AccessDenied
        );
        assert_eq!(
            classify_os_error(32, std::io::ErrorKind::Other),
            ScanErrorKind::Transient
        );
    }

    #[test]
    fn an_unknown_code_falls_back_to_the_portable_kind() {
        assert_eq!(
            classify_os_error(0, std::io::ErrorKind::TimedOut),
            ScanErrorKind::Transient
        );
        assert_eq!(
            classify_os_error(0, std::io::ErrorKind::Other),
            ScanErrorKind::Other
        );
    }
}
