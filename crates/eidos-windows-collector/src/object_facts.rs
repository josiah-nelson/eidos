//! Size and depth for changed objects, read **without ever opening the
//! objects themselves**.
//!
//! This exists because the obvious implementation is actively harmful. The
//! USN lane wants a size for every file closed after a write, and the direct
//! way to get one is to open the file by its reference number and ask. That
//! is what the lane used to do, and it broke other software on the host.
//!
//! On Windows an open handle turns a concurrent delete into a *delete
//! pending*: the name stays occupied until the last handle closes, and
//! anything trying to create a file at that path meanwhile fails with
//! `ERROR_ACCESS_DENIED`. Full sharing flags do not avoid this -
//! `FILE_SHARE_DELETE` is what permits the delete, not what releases the
//! name. Nothing the observer can pass makes holding a handle free.
//!
//! Worse, the lane asked at the worst possible instant. Facts are wanted for
//! `REASON_CLOSE` records, so the collector reached for the file in the
//! moment its writer let go of it - exactly when a build, a compiler, or an
//! index writer is about to delete and recreate that path. The observatory
//! was perturbing the workload it exists to measure.
//!
//! So the rule for the always-on lanes is absolute: **never open an observed
//! file**. Sizes come from enumerating the *parent directory*, which hands
//! back the file id and size of every child in one call. That costs one
//! short-lived handle on a directory instead of one handle per file, it
//! amortises across a whole batch, and directories are not what a churning
//! workload is deleting and recreating.
//!
//! (The content probe is the deliberate exception: opening files is its
//! entire purpose. It is off by default, and its cost is the price of asking
//! for it. Nothing here is on that path.)

use lru::LruCache;
use std::collections::HashSet;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    ExtendedFileIdType, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, OpenFileById,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_EXTD_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

use eidos_scanner::usn::VolumeHandle;

/// Enumeration buffer. Large enough that an ordinary directory comes back in
/// one or two calls, small enough to sit on the stack of the reader thread.
const ENUM_BUFFER: usize = 64 * 1024;

/// Stop enumerating a directory after this many entries. A directory with a
/// million children is not worth walking for one file's size bucket, and the
/// handle should not be held that long. Callers get `None` and carry on.
const MAX_ENTRIES_PER_DIRECTORY: usize = 20_000;

/// An open directory handle, closed on drop.
struct Directory(HANDLE);

impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: opened below and owned here; closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Open a directory by reference number, for listing only.
///
/// `FILE_LIST_DIRECTORY` plus `FILE_FLAG_BACKUP_SEMANTICS` is the minimum
/// that allows enumeration; no data access is requested, and the handle lives
/// only as long as the enumeration below.
fn open_directory(vol: &VolumeHandle, frn: u128) -> Option<Directory> {
    let mut descriptor = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: ExtendedFileIdType,
        ..Default::default()
    };
    descriptor.Anonymous.ExtendedFileId = FILE_ID_128 {
        Identifier: frn.to_le_bytes(),
    };
    // SAFETY: descriptor fully initialised; the volume handle outlives the call.
    let handle = unsafe {
        OpenFileById(
            vol.raw(),
            &descriptor,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    (handle != INVALID_HANDLE_VALUE).then_some(Directory(handle))
}

/// The directory's own path, for the depth of the objects inside it.
fn directory_path(dir: &Directory) -> Option<String> {
    let mut buffer = vec![0u16; 1024];
    // SAFETY: handle is open; the buffer length is passed in characters.
    let written = unsafe {
        GetFinalPathNameByHandleW(dir.0, buffer.as_mut_ptr(), buffer.len() as u32 - 1, 0)
    };
    if written == 0 || written as usize >= buffer.len() {
        return None;
    }
    buffer.truncate(written as usize);
    Some(String::from_utf16_lossy(&buffer))
}

/// Walk a directory once, handing every child's reference number and size to
/// `sink`. Stops early when `sink` returns false or the entry cap is reached.
///
/// The handle is open only for the duration of this call, and it is a handle
/// on the directory - never on the children whose sizes are being read.
fn for_each_child(dir: &Directory, mut sink: impl FnMut(u128, u64) -> bool) {
    // Allocated as u64 so the base is 8-aligned: the kernel writes a chain of
    // 8-aligned structures, and reading them through a byte buffer whose
    // start happened to be odd would be unaligned.
    let mut buffer = vec![0u64; ENUM_BUFFER / 8];
    let base = buffer.as_mut_ptr().cast::<u8>();
    let mut class = FileIdExtdDirectoryRestartInfo;
    let mut seen = 0usize;
    loop {
        // SAFETY: the buffer outlives the call and its length is passed in bytes.
        let ok =
            unsafe { GetFileInformationByHandleEx(dir.0, class, base.cast(), ENUM_BUFFER as u32) };
        if ok == 0 {
            // ERROR_NO_MORE_FILES ends the walk; anything else ends it too,
            // because a partial answer is still a usable one.
            return;
        }
        class = FileIdExtdDirectoryInfo;

        let mut offset = 0usize;
        loop {
            // SAFETY: the kernel filled this buffer with a chain of
            // FILE_ID_EXTD_DIR_INFO, each at `NextEntryOffset` from the last.
            let entry = unsafe { &*(base.add(offset) as *const FILE_ID_EXTD_DIR_INFO) };
            let frn = u128::from_le_bytes(entry.FileId.Identifier);
            if !sink(frn, entry.EndOfFile as u64) {
                return;
            }
            seen += 1;
            if seen >= MAX_ENTRIES_PER_DIRECTORY {
                return;
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
}

/// Depth of a directory below its volume root: `\\?\D:\a\b` is 2.
pub fn path_depth(path: &str) -> usize {
    let stripped = path
        .strip_prefix(r"\\?\")
        .unwrap_or(path)
        .trim_end_matches('\\');
    let Some((_, rest)) = stripped.split_once('\\') else {
        return 0;
    };
    if rest.is_empty() {
        0
    } else {
        rest.matches('\\').count() + 1
    }
}

/// Per-batch fact lookup: caches that live across batches, plus the set of
/// parents already walked in *this* batch.
///
/// The per-batch set is what keeps a churning directory cheap. A build
/// directory can produce thousands of close records in one batch; without it
/// each one would re-walk the same parent.
pub struct Lookup<'a> {
    sizes: &'a mut LruCache<u128, u64>,
    depths: &'a mut LruCache<u128, usize>,
    walked: HashSet<u128>,
}

impl<'a> Lookup<'a> {
    pub fn new(sizes: &'a mut LruCache<u128, u64>, depths: &'a mut LruCache<u128, usize>) -> Self {
        Self {
            sizes,
            depths,
            walked: HashSet::new(),
        }
    }

    /// Size and depth for one changed object, opening nothing but its parent.
    pub fn facts(
        &mut self,
        vol: &VolumeHandle,
        frn: u128,
        parent_frn: u128,
    ) -> (Option<u64>, Option<usize>) {
        if let Some(size) = self.sizes.get(&frn).copied() {
            return (Some(size), self.depth_only(vol, parent_frn));
        }
        // One walk per parent per batch, filling the size of every sibling
        // that changed in the same batch along the way.
        if self.walked.insert(parent_frn) {
            self.walk(vol, parent_frn);
        }
        (
            self.sizes.get(&frn).copied(),
            self.depths.get(&parent_frn).copied(),
        )
    }

    fn depth_only(&mut self, vol: &VolumeHandle, parent_frn: u128) -> Option<usize> {
        if let Some(depth) = self.depths.get(&parent_frn).copied() {
            return Some(depth);
        }
        if self.walked.insert(parent_frn) {
            self.walk(vol, parent_frn);
        }
        self.depths.get(&parent_frn).copied()
    }

    /// Open the parent once: take its depth from its own path, and the size
    /// of every child from one enumeration.
    fn walk(&mut self, vol: &VolumeHandle, parent_frn: u128) {
        let Some(dir) = open_directory(vol, parent_frn) else {
            return;
        };
        if let Some(depth) = directory_path(&dir).as_deref().map(path_depth) {
            self.depths.put(parent_frn, depth);
        }
        let sizes = &mut *self.sizes;
        for_each_child(&dir, |frn, size| {
            sizes.put(frn, size);
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::num::NonZeroUsize;
    use std::os::windows::io::AsRawHandle;

    fn caches() -> (LruCache<u128, u64>, LruCache<u128, usize>) {
        (
            LruCache::new(NonZeroUsize::new(1024).unwrap()),
            LruCache::new(NonZeroUsize::new(1024).unwrap()),
        )
    }

    /// The reference number of a path. The tests may open files freely - they
    /// are standing in for the workload, not for the observer.
    fn frn_of(path: &std::path::Path) -> Option<u128> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FileIdInfo, FILE_ID_INFO};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .ok()?;
        let mut info = FILE_ID_INFO {
            VolumeSerialNumber: 0,
            FileId: FILE_ID_128 {
                Identifier: [0; 16],
            },
        };
        // SAFETY: `info` is a live FILE_ID_INFO of the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        (ok != 0).then(|| u128::from_le_bytes(info.FileId.Identifier))
    }

    /// The volume a temporary directory lives on, plus a written file's
    /// reference number and its parent's.
    fn subject(
        dir: &std::path::Path,
        name: &str,
        bytes: &[u8],
    ) -> Option<(VolumeHandle, u128, u128)> {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).ok()?;
        file.write_all(bytes).ok()?;
        drop(file);
        let text = dir.to_string_lossy().to_string();
        // Drive-letter paths only; a temporary directory is always one.
        let root = format!("{}\\", text.get(..2)?);
        let vol = VolumeHandle::open(&root).ok()?;
        Some((vol, frn_of(&path)?, frn_of(dir)?))
    }

    /// The invariant, stated behaviourally: resolving facts must not disturb
    /// the workload being observed.
    ///
    /// A workload that creates, closes, deletes and recreates the same paths
    /// is the pattern that exposed the original defect - it is what build
    /// tools and index writers do. This hammers the fact lookup against
    /// exactly that while it runs, and requires the *workload* to succeed
    /// every single time. What the lookup manages to learn is not the
    /// assertion; what it costs the host is.
    #[test]
    fn a_churning_workload_is_never_disturbed() {
        const ROUNDS: usize = 400;
        let temp = tempfile::tempdir().unwrap();
        let Some((vol, frn, parent)) = subject(temp.path(), "seed.bin", &[0u8; 64]) else {
            return;
        };

        let dir = temp.path().to_path_buf();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workload_stop = stop.clone();
        let workload = std::thread::spawn(move || {
            let mut failures = Vec::new();
            for round in 0..ROUNDS {
                let path = dir.join(format!("churn-{}.bin", round % 8));
                // create -> write -> close -> delete -> recreate, the shape
                // that made an index writer fail with ACCESS_DENIED.
                for step in 0..2 {
                    match std::fs::File::create(&path) {
                        Ok(mut file) => {
                            use std::io::Write;
                            let _ = file.write_all(&[step as u8; 512]);
                        }
                        Err(error) => failures.push(format!("{path:?} create: {error}")),
                    }
                    if let Err(error) = std::fs::remove_file(&path) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            failures.push(format!("{path:?} remove: {error}"));
                        }
                    }
                }
            }
            workload_stop.store(true, std::sync::atomic::Ordering::Release);
            failures
        });

        // Walk the churning directory as hard as the reader ever would.
        let mut walks = 0u64;
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            let (mut sizes, mut depths) = caches();
            let mut lookup = Lookup::new(&mut sizes, &mut depths);
            let _ = lookup.facts(&vol, frn, parent);
            walks += 1;
        }

        let failures = workload.join().unwrap();
        assert!(
            failures.is_empty(),
            "the observed workload must not fail; {} of its operations did after {walks} walks:
{}",
            failures.len(),
            failures.join(
                "
"
            )
        );
    }

    /// One walk per parent per batch, however many siblings changed.
    #[test]
    fn siblings_share_one_directory_walk() {
        let temp = tempfile::tempdir().unwrap();
        let Some((vol, first, parent)) = subject(temp.path(), "a.bin", &[1u8; 10]) else {
            return;
        };
        let Some((_, second, _)) = subject(temp.path(), "b.bin", &[2u8; 20]) else {
            return;
        };

        let (mut sizes, mut depths) = caches();
        let mut lookup = Lookup::new(&mut sizes, &mut depths);
        assert_eq!(lookup.facts(&vol, first, parent).0, Some(10));
        assert_eq!(lookup.facts(&vol, second, parent).0, Some(20));
        assert_eq!(
            lookup.walked.len(),
            1,
            "the second sibling must not walk the parent again"
        );
    }

    #[test]
    fn depth_counts_components_below_the_root() {
        assert_eq!(path_depth(r"\\?\D:"), 0);
        assert_eq!(path_depth(r"\\?\D:\"), 0);
        assert_eq!(path_depth(r"\\?\D:\a"), 1);
        assert_eq!(path_depth(r"\\?\D:\a\b\c"), 3);
        assert_eq!(path_depth(r"\\?\Volume{x}\a\b"), 2);
    }
}
