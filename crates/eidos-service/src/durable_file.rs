//! One durable replacement and removal for the settings files a coordinated
//! resource operation touches, so the journal and the component files it
//! replays can never become durable on different terms.
//!
//! The new bytes go to a `<name>.tmp` sibling that is fsynced and renamed over
//! the old name, and the containing directory is fsynced afterwards on
//! platforms that need it to make the new directory entry durable. Windows has
//! no portable directory sync, so there the entry follows NTFS rename
//! semantics; `docs/coordinated-resource-settings.md` states that boundary
//! rather than claiming power-loss atomicity the platform does not give.

use std::{
    io,
    path::{Path, PathBuf},
};

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    name.into()
}

/// Replace `path` with `bytes`.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(path);
    let write = || -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = write() {
        // Never leave a half-written temporary behind for the next save (or
        // an operator reading the data directory) to trip over.
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    sync_parent(path)
}

/// Remove `path`, making the removal durable on the same terms.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)?;
    sync_parent(path)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    std::fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_replacement_leaves_neither_a_temporary_nor_a_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        replace(&path, b"{\"kept\":1}").unwrap();

        // A directory in the temporary's place fails the write the same way a
        // read-only or full volume does.
        std::fs::create_dir(temp_path(&path)).unwrap();
        assert!(replace(&path, b"{\"kept\":2}").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"kept\":1}");

        std::fs::remove_dir(temp_path(&path)).unwrap();
        replace(&path, b"{\"kept\":2}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"kept\":2}");
        assert!(!temp_path(&path).exists());
        remove(&path).unwrap();
        assert!(!path.exists());
    }
}
