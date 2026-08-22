//! Filesystem enumeration and change-feed adapters.
//!
//! `entry` defines the platform-neutral contract: a [`RawEntry`] is what any
//! lister yields for a directory child. `walk` drives a bounded parallel
//! traversal over any [`DirectoryLister`]. `win` implements the Windows
//! fast path (`FileIdExtdDirectoryInfo` batches with 128-bit file IDs and
//! allocation sizes) and volume capability detection; `std_lister` is the
//! portable fallback used on other platforms and in tests.

pub mod entry;
pub mod error;
pub mod std_lister;
pub mod walk;

#[cfg(windows)]
pub mod usn;
#[cfg(windows)]
pub mod win;

pub use entry::*;
pub use error::*;
pub use walk::*;

/// Canonical user-facing form of a source root.
pub fn normalize_root(path: &str) -> String {
    #[cfg(windows)]
    {
        win::normalize_root(path)
    }
    #[cfg(not(windows))]
    {
        let s = path.trim_end_matches('/');
        if s.is_empty() {
            "/".to_string()
        } else {
            s.to_string()
        }
    }
}

/// Return the best lister for the current platform.
pub fn default_lister() -> Box<dyn DirectoryLister> {
    #[cfg(windows)]
    {
        Box::new(win::WinLister::new())
    }
    #[cfg(not(windows))]
    {
        Box::new(std_lister::StdLister)
    }
}
