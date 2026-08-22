//! Windows directory lister and volume capability detection.
//!
//! Uses `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` to fetch
//! batches of children with 128-bit file IDs, allocation sizes, all four
//! timestamps, attributes, and reparse tags in one call per 64 KiB. Falls back
//! to `FileIdBothDirectoryInfo` (64-bit IDs) and `FileFullDirectoryInfo` (no
//! IDs) for filesystems/servers that reject the richer classes.

use crate::entry::{DirectoryLister, DriveType, RawEntry, VolumeInfo};
use crate::error::{classify_os_error, ScanError, ScanErrorKind};
use eidos_domain::{FileAttributes, IdentityConfidence, NativeIdentity, ObjectKind, UnixNanos};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo,
    FileIdInfo, GetDiskFreeSpaceW, GetDriveTypeW, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, GetVolumePathNameW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO,
    FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

// Filesystem flag bits (winnt.h). Defined locally; values are ABI-stable.
pub const FILE_FILE_COMPRESSION: u32 = 0x0000_0010;
pub const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x0000_0040;
pub const FILE_SUPPORTS_REPARSE_POINTS: u32 = 0x0000_0080;
pub const FILE_SUPPORTS_HARD_LINKS: u32 = 0x0040_0000;
pub const FILE_SUPPORTS_OPEN_BY_FILE_ID: u32 = 0x0100_0000;
pub const FILE_SUPPORTS_USN_JOURNAL: u32 = 0x0200_0000;

const DRIVE_NO_ROOT_DIR: u32 = 1;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

const BATCH_BYTES: usize = 64 * 1024;

/// Owned Win32 handle that closes on drop.
pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: handle is valid and owned by this wrapper.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Encode a path as a NUL-terminated UTF-16 string, applying the `\\?\`
/// extended-length prefix when it is absent.
pub fn to_wide_extended(path: &Path) -> Vec<u16> {
    let p = extended_path(path);
    let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
    v.push(0);
    v
}

/// Return `path` in extended-length form (`\\?\C:\...` or `\\?\UNC\srv\share`).
/// Forward slashes are normalised because the `\\?\` prefix disables the
/// Win32 path normaliser.
pub fn extended_path(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy().replace('/', "\\");
    if s.starts_with("\\\\?\\") || s.starts_with("\\\\.\\") {
        return PathBuf::from(s);
    }
    if let Some(rest) = s.strip_prefix("\\\\") {
        return PathBuf::from(format!("\\\\?\\UNC\\{rest}"));
    }
    PathBuf::from(format!("\\\\?\\{s}"))
}

/// Canonical user-facing form of a source root: backslashes, no
/// extended-length prefix, trailing separator only for drive roots.
pub fn normalize_root(path: &str) -> String {
    let mut s = display_path(Path::new(&path.replace('/', "\\")));
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    if s.len() == 2 && s.as_bytes()[1] == b':' {
        s.push('\\');
    }
    s
}

/// Classify by attributes. Only *directory* reparse points (junctions, mount
/// points, directory symlinks) become `Reparse` — they are never traversed.
/// File reparse points (WIM/dedup-backed data, cloud placeholders, file
/// symlinks) are file-like objects with real sizes; content policy decides
/// per reparse tag whether their bytes are read.
pub fn object_kind(attributes: FileAttributes) -> ObjectKind {
    if attributes.is_directory() {
        if attributes.is_reparse() {
            ObjectKind::Reparse
        } else {
            ObjectKind::Directory
        }
    } else {
        ObjectKind::File
    }
}

/// Remove the extended-length prefix for display.
pub fn display_path(path: &Path) -> String {
    let s = path.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        return rest.to_string();
    }
    s.into_owned()
}

fn last_error(path: &Path, what: &str) -> ScanError {
    // SAFETY: trivially safe FFI call.
    let code = unsafe { GetLastError() } as i32;
    let io = std::io::Error::from_raw_os_error(code);
    ScanError::new(
        classify_os_error(code, io.kind()),
        code,
        format!("{what}: {io}"),
        path,
    )
}

/// Open a directory handle suitable for enumeration and information queries.
pub fn open_directory(path: &Path) -> Result<Handle, ScanError> {
    let wide = to_wide_extended(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error(path, "CreateFileW"));
    }
    Ok(Handle(h))
}

fn file_id_info(h: &Handle, path: &Path) -> Result<FILE_ID_INFO, ScanError> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: buffer size matches the struct.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.0,
            FileIdInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error(path, "GetFileInformationByHandleEx(FileIdInfo)"));
    }
    Ok(info)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InfoClass {
    Extd,
    Both,
    Full,
}

impl InfoClass {
    fn classes(self) -> (FILE_INFO_BY_HANDLE_CLASS, FILE_INFO_BY_HANDLE_CLASS) {
        match self {
            InfoClass::Extd => (FileIdExtdDirectoryRestartInfo, FileIdExtdDirectoryInfo),
            InfoClass::Both => (FileIdBothDirectoryRestartInfo, FileIdBothDirectoryInfo),
            InfoClass::Full => (FileFullDirectoryRestartInfo, FileFullDirectoryInfo),
        }
    }
    fn from_i32(v: i32) -> Self {
        match v {
            1 => InfoClass::Both,
            2 => InfoClass::Full,
            _ => InfoClass::Extd,
        }
    }
    fn to_i32(self) -> i32 {
        match self {
            InfoClass::Extd => 0,
            InfoClass::Both => 1,
            InfoClass::Full => 2,
        }
    }
    fn next(self) -> Option<Self> {
        match self {
            InfoClass::Extd => Some(InfoClass::Both),
            InfoClass::Both => Some(InfoClass::Full),
            InfoClass::Full => None,
        }
    }
}

/// Windows lister. Cheap to clone/share; remembers which information class
/// last succeeded so fallbacks are only probed once per volume type.
pub struct WinLister {
    preferred: AtomicI32,
}

impl Default for WinLister {
    fn default() -> Self {
        Self::new()
    }
}

impl WinLister {
    pub fn new() -> Self {
        Self {
            preferred: AtomicI32::new(InfoClass::Extd.to_i32()),
        }
    }

    fn list_with_class(
        &self,
        h: &Handle,
        path: &Path,
        class: InfoClass,
        volume_serial: u64,
        buf: &mut [u8],
    ) -> Result<Vec<RawEntry>, ScanError> {
        let (restart, cont) = class.classes();
        let mut out = Vec::with_capacity(64);
        let mut first = true;
        loop {
            let cls = if first { restart } else { cont };
            // SAFETY: buffer is BATCH_BYTES, 8-byte aligned via Vec<u8> with
            // manual alignment check below; the API writes at most the given size.
            let ok = unsafe {
                GetFileInformationByHandleEx(h.0, cls, buf.as_mut_ptr() as *mut _, buf.len() as u32)
            };
            first = false;
            if ok == 0 {
                // SAFETY: trivially safe.
                let code = unsafe { GetLastError() };
                if code == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(last_error(path, "GetFileInformationByHandleEx(directory)"));
            }
            let mut offset = 0usize;
            loop {
                // SAFETY: the kernel guarantees NextEntryOffset chains stay within
                // the written region; we additionally bounds-check every read.
                let (next, entry) =
                    unsafe { parse_entry(buf, offset, class, volume_serial, path)? };
                if let Some(e) = entry {
                    out.push(e);
                }
                if next == 0 {
                    break;
                }
                offset += next;
                if offset >= buf.len() {
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// Parse one entry at `offset`. Returns `(next_entry_offset, entry)`.
#[allow(clippy::type_complexity)]
unsafe fn parse_entry(
    buf: &[u8],
    offset: usize,
    class: InfoClass,
    volume_serial: u64,
    path: &Path,
) -> Result<(usize, Option<RawEntry>), ScanError> {
    let base = buf.as_ptr().add(offset);
    let need = match class {
        InfoClass::Extd => std::mem::size_of::<FILE_ID_EXTD_DIR_INFO>(),
        InfoClass::Both => std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>(),
        InfoClass::Full => std::mem::size_of::<FILE_FULL_DIR_INFO>(),
    };
    if offset + need > buf.len() {
        return Err(ScanError::new(
            ScanErrorKind::Other,
            0,
            "directory info entry truncated",
            path,
        ));
    }
    let (next, name_off, name_len, attrs, size, alloc, ct, at, wt, cht, tag, id): (
        usize,
        usize,
        usize,
        u32,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        u32,
        Option<NativeIdentity>,
    ) = match class {
        InfoClass::Extd => {
            let e = &*(base as *const FILE_ID_EXTD_DIR_INFO);
            let id_bytes = e.FileId.Identifier;
            let id = u128::from_le_bytes(id_bytes);
            (
                e.NextEntryOffset as usize,
                std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
                e.FileNameLength as usize,
                e.FileAttributes,
                e.EndOfFile,
                e.AllocationSize,
                e.CreationTime,
                e.LastAccessTime,
                e.LastWriteTime,
                e.ChangeTime,
                e.ReparsePointTag,
                Some(NativeIdentity::from_u128(
                    volume_serial,
                    id,
                    IdentityConfidence::Native,
                )),
            )
        }
        InfoClass::Both => {
            let e = &*(base as *const FILE_ID_BOTH_DIR_INFO);
            (
                e.NextEntryOffset as usize,
                std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName),
                e.FileNameLength as usize,
                e.FileAttributes,
                e.EndOfFile,
                e.AllocationSize,
                e.CreationTime,
                e.LastAccessTime,
                e.LastWriteTime,
                e.ChangeTime,
                if e.FileAttributes & FileAttributes::REPARSE_POINT != 0 {
                    e.EaSize
                } else {
                    0
                },
                Some(NativeIdentity::from_u128(
                    volume_serial,
                    e.FileId as u64 as u128,
                    IdentityConfidence::Weak,
                )),
            )
        }
        InfoClass::Full => {
            let e = &*(base as *const FILE_FULL_DIR_INFO);
            (
                e.NextEntryOffset as usize,
                std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName),
                e.FileNameLength as usize,
                e.FileAttributes,
                e.EndOfFile,
                e.AllocationSize,
                e.CreationTime,
                e.LastAccessTime,
                e.LastWriteTime,
                e.ChangeTime,
                if e.FileAttributes & FileAttributes::REPARSE_POINT != 0 {
                    e.EaSize
                } else {
                    0
                },
                None,
            )
        }
    };
    if offset + name_off + name_len > buf.len() {
        return Err(ScanError::new(
            ScanErrorKind::Other,
            0,
            "directory entry name truncated",
            path,
        ));
    }
    let name_ptr = base.add(name_off) as *const u16;
    let name_units = std::slice::from_raw_parts(name_ptr, name_len / 2);
    if name_units == [b'.' as u16] || name_units == [b'.' as u16, b'.' as u16] {
        return Ok((next, None));
    }
    let (name, name_lossy) = match String::from_utf16(name_units) {
        Ok(s) => (s, false),
        Err(_) => (String::from_utf16_lossy(name_units), true),
    };
    let attributes = FileAttributes(attrs);
    let kind = object_kind(attributes);
    let ft = |v: i64| {
        if v == 0 {
            None
        } else {
            Some(UnixNanos::from_filetime_ticks(v))
        }
    };
    Ok((
        next,
        Some(RawEntry {
            name,
            name_lossy,
            kind,
            attributes,
            size: if kind == ObjectKind::File {
                size.max(0) as u64
            } else {
                0
            },
            allocated: Some(alloc.max(0) as u64),
            created: ft(ct),
            modified: ft(wt),
            changed: ft(cht),
            accessed: ft(at),
            native_id: id,
            reparse_tag: tag,
        }),
    ))
}

/// 8-byte aligned batch buffer: the directory-information structures contain
/// `i64` fields, so the buffer base must be 8-byte aligned.
struct AlignedBatch(Vec<u64>);

impl AlignedBatch {
    fn new() -> Self {
        Self(vec![0u64; BATCH_BYTES / 8])
    }
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: u64 -> u8 view of the same allocation; length in bytes.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, self.0.len() * 8) }
    }
}

thread_local! {
    static BATCH: std::cell::RefCell<AlignedBatch> = std::cell::RefCell::new(AlignedBatch::new());
}

impl DirectoryLister for WinLister {
    fn list(&self, dir: &Path) -> Result<Vec<RawEntry>, ScanError> {
        let h = open_directory(dir)?;
        let volume_serial = match file_id_info(&h, dir) {
            Ok(i) => i.VolumeSerialNumber,
            Err(_) => 0,
        };
        let mut class = InfoClass::from_i32(self.preferred.load(Ordering::Relaxed));
        BATCH.with(|b| {
            let mut batch = b.borrow_mut();
            let buf = batch.as_bytes_mut();
            loop {
                match self.list_with_class(&h, dir, class, volume_serial, buf) {
                    Ok(v) => return Ok(v),
                    Err(e) if e.kind == ScanErrorKind::Unsupported => {
                        if let Some(next) = class.next() {
                            tracing::debug!(path = %dir.display(), from = ?class, to = ?next, "directory info class fallback");
                            class = next;
                            self.preferred.store(class.to_i32(), Ordering::Relaxed);
                            continue;
                        }
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            }
        })
    }

    fn volume_info(&self, root: &Path) -> Result<VolumeInfo, ScanError> {
        let h = open_directory(root)?;
        let mut serial32: u32 = 0;
        let mut max_component: u32 = 0;
        let mut flags: u32 = 0;
        let mut vol_name = [0u16; 261];
        let mut fs_name = [0u16; 261];
        // SAFETY: buffers sized as declared.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                h.0,
                vol_name.as_mut_ptr(),
                vol_name.len() as u32,
                &mut serial32,
                &mut max_component,
                &mut flags,
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error(root, "GetVolumeInformationByHandleW"));
        }
        let serial64 = file_id_info(&h, root)
            .map(|i| i.VolumeSerialNumber)
            .unwrap_or(serial32 as u64);
        let wide = to_wide_extended(root);
        let mut vol_root = [0u16; 1024];
        // SAFETY: buffers sized as declared.
        let ok = unsafe {
            GetVolumePathNameW(wide.as_ptr(), vol_root.as_mut_ptr(), vol_root.len() as u32)
        };
        let volume_root = if ok != 0 {
            display_path(Path::new(&wide_to_string(&vol_root)))
        } else {
            display_path(root)
        };
        let mut root_wide: Vec<u16> = OsStr::new(&volume_root).encode_wide().collect();
        if !volume_root.ends_with('\\') {
            root_wide.push('\\' as u16);
        }
        root_wide.push(0);
        // SAFETY: NUL-terminated.
        let dt = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        let drive_type = match dt {
            DRIVE_NO_ROOT_DIR => DriveType::NoRootDir,
            DRIVE_REMOVABLE => DriveType::Removable,
            DRIVE_FIXED => DriveType::Fixed,
            DRIVE_REMOTE => DriveType::Remote,
            DRIVE_CDROM => DriveType::CdRom,
            DRIVE_RAMDISK => DriveType::RamDisk,
            _ => DriveType::Unknown,
        };
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: NUL-terminated root path; out-params valid.
        let ok = unsafe {
            GetDiskFreeSpaceW(
                root_wide.as_ptr(),
                &mut spc,
                &mut bps,
                &mut free,
                &mut total,
            )
        };
        let bytes_per_cluster = if ok != 0 { spc.saturating_mul(bps) } else { 0 };
        Ok(VolumeInfo {
            volume_serial: serial64,
            filesystem: wide_to_string(&fs_name),
            volume_name: wide_to_string(&vol_name),
            fs_flags: flags,
            drive_type,
            supports_file_ids: flags & FILE_SUPPORTS_OPEN_BY_FILE_ID != 0,
            supports_usn: flags & FILE_SUPPORTS_USN_JOURNAL != 0,
            supports_hard_links: flags & FILE_SUPPORTS_HARD_LINKS != 0,
            supports_reparse_points: flags & FILE_SUPPORTS_REPARSE_POINTS != 0,
            supports_sparse: flags & FILE_SUPPORTS_SPARSE_FILES != 0,
            bytes_per_cluster,
            volume_root,
        })
    }

    fn stat(&self, path: &Path) -> Result<RawEntry, ScanError> {
        let s = crate::usn::snapshot_path(path)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(RawEntry {
            name,
            name_lossy: false,
            kind: s.kind,
            attributes: s.attributes,
            size: s.size,
            allocated: Some(s.allocated),
            created: s.created,
            modified: s.modified,
            changed: s.changed,
            accessed: s.accessed,
            native_id: Some(s.native),
            reparse_tag: s.reparse_tag,
        })
    }

    fn name(&self) -> &'static str {
        "windows"
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn extended_paths() {
        assert_eq!(
            extended_path(Path::new("C:\\x")).to_string_lossy(),
            "\\\\?\\C:\\x"
        );
        assert_eq!(
            extended_path(Path::new("\\\\srv\\share\\y")).to_string_lossy(),
            "\\\\?\\UNC\\srv\\share\\y"
        );
        assert_eq!(
            extended_path(Path::new("\\\\?\\C:\\x")).to_string_lossy(),
            "\\\\?\\C:\\x"
        );
        assert_eq!(
            display_path(Path::new("\\\\?\\UNC\\srv\\share")),
            "\\\\srv\\share"
        );
        assert_eq!(display_path(Path::new("\\\\?\\C:\\x")), "C:\\x");
        assert_eq!(
            extended_path(Path::new("G:/")).to_string_lossy(),
            "\\\\?\\G:\\"
        );
        assert_eq!(
            extended_path(Path::new("G:/a/b")).to_string_lossy(),
            "\\\\?\\G:\\a\\b"
        );
    }

    #[test]
    fn root_normalisation() {
        assert_eq!(normalize_root("G:/"), "G:\\");
        assert_eq!(normalize_root("G:"), "G:\\");
        assert_eq!(normalize_root("G:\\"), "G:\\");
        assert_eq!(normalize_root("G:\\Tools\\"), "G:\\Tools");
        assert_eq!(normalize_root("\\\\?\\G:\\Tools"), "G:\\Tools");
        assert_eq!(
            normalize_root("//fileserver/share/"),
            "\\\\fileserver\\share"
        );
    }

    #[test]
    fn lists_temp_tree_with_ids_and_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), vec![b'x'; 5000]).unwrap();
        std::fs::write(tmp.path().join("empty.txt"), b"").unwrap();
        let lister = WinLister::new();
        let entries = lister.list(tmp.path()).unwrap();
        assert_eq!(entries.len(), 3);
        let names: HashSet<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains("sub") && names.contains("a.txt") && names.contains("empty.txt"));
        let a = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(a.size, 5000);
        assert!(a.allocated.unwrap() >= 5000);
        assert!(a.native_id.is_some());
        assert!(a.modified.is_some());
        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_traversable_dir());
        let vi = lister.volume_info(tmp.path()).unwrap();
        assert!(!vi.filesystem.is_empty());
        assert_ne!(vi.volume_serial, 0);
        // All entries on the temp volume share its serial.
        for e in &entries {
            assert_eq!(e.native_id.unwrap().volume_serial, vi.volume_serial);
        }
    }

    #[test]
    fn hard_links_share_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        std::fs::write(&a, b"hello").unwrap();
        std::fs::hard_link(&a, tmp.path().join("b.txt")).unwrap();
        let entries = WinLister::new().list(tmp.path()).unwrap();
        let ids: Vec<_> = entries
            .iter()
            .map(|e| e.native_id.unwrap().file_id_u128())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }

    #[test]
    fn missing_dir_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = WinLister::new()
            .list(&tmp.path().join("missing"))
            .unwrap_err();
        assert_eq!(err.kind, ScanErrorKind::NotFound);
    }
}
