//! macOS enumeration fast path built on `getattrlistbulk(2)`.
//!
//! One syscall returns name, object type, identity, sizes, timestamps, and BSD
//! flags for many children at once, the way `FileIdExtdDirectoryInfo` batches
//! do on Windows. `readdir(3)` is competitive when only names are needed, but
//! this scanner needs metadata for every entry and `readdir` + `lstat` costs a
//! syscall per child.
//!
//! Two deliberate restrictions:
//!
//! - The bulk path runs on locally mounted volumes only. macOS SMB restarts a
//!   directory enumeration from the beginning when its entry cache is refilled
//!   (Apple FB15497909), which makes a bulk listing repeat entries; remote
//!   volumes therefore fall back to the portable `readdir` lister, which Apple
//!   also measures as the faster option there.
//! - Any parse that does not consume exactly one attribute group falls back to
//!   the portable lister for that directory rather than guessing, so a
//!   filesystem that packs the buffer differently degrades instead of
//!   inventing metadata.
//!
//! macOS facts are recorded as themselves, never normalised into NTFS
//! behaviour: allocation size covers all forks, mount points are surfaced the
//! way Windows volume mount points are (a directory that is not traversed
//! automatically), firmlinks stay traversable so `/Users` is reached once
//! through its user-facing path, and dataless (cloud placeholder) files carry
//! the offline attributes that keep content extraction from hydrating them.

use crate::entry::{DirectoryLister, DriveType, NativeFeed, RawEntry, VolumeInfo};
use crate::error::{ScanError, ScanErrorKind};
use crate::std_lister::StdLister;
use eidos_domain::{FileAttributes, IdentityConfidence, NativeIdentity, ObjectKind, UnixNanos};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Apple's `fts(3)` reads two pages at a time and the bulk call fails with
/// `ERANGE` if a single group does not fit; 64 KiB keeps large batches per
/// syscall without a meaningful memory cost (one buffer per walker thread).
const ATTR_BUF_LEN: usize = 64 * 1024;

/// `ATTR_CMN_ERROR` is absent from the `libc` crate.
const ATTR_CMN_ERROR: libc::attrgroup_t = 0x2000_0000;
/// `MNT_REMOVABLE` from `<sys/mount.h>`; also absent from `libc`.
const MNT_REMOVABLE: u32 = 0x0000_0200;
/// `SF_DATALESS` from `<sys/stat.h>`: the object's data lives elsewhere and
/// reading it would trigger materialisation.
const SF_DATALESS: u32 = 0x4000_0000;

/// `enum vtype` from `<sys/vnode.h>`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// Requested common attributes, in the order the kernel packs them.
/// `ATTR_CMN_RETURNED_ATTRS` is always first and `ATTR_CMN_ERROR` always
/// second; the rest follow ascending bit order.
const COMMON_ATTRS: libc::attrgroup_t = libc::ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_CRTIME
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_CHGTIME
    | libc::ATTR_CMN_ACCTIME
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_FILEID;
const DIR_ATTRS: libc::attrgroup_t = libc::ATTR_DIR_MOUNTSTATUS;
const FILE_ATTRS: libc::attrgroup_t = libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;

/// Every requested attribute occupies its slot, valid or not, so the layout is
/// fixed and `ATTR_CMN_RETURNED_ATTRS` decides which values mean anything.
const BULK_OPTIONS: u64 = (libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_NOFOLLOW) as u64;

/// Volume attributes, requested in packing order.
const VOL_ATTRS: libc::attrgroup_t = libc::ATTR_VOL_INFO
    | libc::ATTR_VOL_MINALLOCATION
    | libc::ATTR_VOL_NAME
    | libc::ATTR_VOL_CAPABILITIES;

fn attr_list(
    common: libc::attrgroup_t,
    vol: libc::attrgroup_t,
    dir: libc::attrgroup_t,
    file: libc::attrgroup_t,
) -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT as libc::c_ushort,
        reserved: 0,
        commonattr: common,
        volattr: vol,
        dirattr: dir,
        fileattr: file,
        forkattr: 0,
    }
}

fn last_error(path: &Path) -> ScanError {
    ScanError::from_io(path, &std::io::Error::last_os_error())
}

fn cstring(path: &Path) -> Result<CString, ScanError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ScanError::new(
            ScanErrorKind::InvalidName,
            0,
            "path contains an interior NUL",
            path,
        )
    })
}

/// Reader over one attribute group. Attributes are aligned to four bytes even
/// when they are 64 bits wide, so every read copies out of the buffer instead
/// of casting a pointer.
struct AttrCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> AttrCursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_ne_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_ne_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_ne_bytes(self.take(8)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_ne_bytes(self.take(8)?.try_into().ok()?))
    }

    fn attribute_set(&mut self) -> Option<libc::attribute_set_t> {
        Some(libc::attribute_set_t {
            commonattr: self.u32()?,
            volattr: self.u32()?,
            dirattr: self.u32()?,
            fileattr: self.u32()?,
            forkattr: self.u32()?,
        })
    }

    /// A `timespec` is two 64-bit fields; only whole nanoseconds are kept.
    fn timespec(&mut self) -> Option<UnixNanos> {
        let secs = self.i64()?;
        let nanos = self.i64()?;
        Some(UnixNanos::new(
            secs.saturating_mul(1_000_000_000).saturating_add(nanos),
        ))
    }

    /// Resolve an `attrreference_t` into the bytes it points at. The offset is
    /// relative to the reference itself, and the data lives after the fixed
    /// fields of the same group.
    fn attr_reference(&mut self) -> Option<&'a [u8]> {
        let base = self.pos;
        let offset = self.i32()?;
        let len = self.u32()? as usize;
        let start = base.checked_add_signed(offset as isize)?;
        let end = start.checked_add(len)?;
        self.buf.get(start..end)
    }
}

/// One entry as the kernel packed it, before it becomes a [`RawEntry`].
struct ParsedEntry<'a> {
    returned: libc::attribute_set_t,
    error: u32,
    name: &'a [u8],
    dev: i32,
    obj_type: u32,
    created: UnixNanos,
    modified: UnixNanos,
    changed: UnixNanos,
    accessed: UnixNanos,
    access_mask: u32,
    flags: u32,
    file_id: u64,
    mount_status: u32,
    allocated: i64,
    data_length: i64,
}

/// Parse one attribute group. `group` starts at its leading length field and
/// covers exactly that many bytes. Returns `None` when the packed layout does
/// not match the requested attribute list, which makes the caller fall back to
/// the portable lister instead of trusting misaligned values.
///
/// `FSOPT_PACK_INVAL_ATTRS` reserves a slot for every *common* attribute, even
/// ones the filesystem cannot answer, so those are read positionally. The
/// directory and file groups are different: the kernel only packs the group
/// that applies to the object's type, so those fields are read exactly when
/// `ATTR_CMN_RETURNED_ATTRS` says they are present.
fn parse_group(group: &[u8]) -> Option<ParsedEntry<'_>> {
    let mut c = AttrCursor::new(group);
    let _length = c.u32()?;
    let returned = c.attribute_set()?;
    let error = c.u32()?;
    let name = c.attr_reference()?;
    let dev = c.i32()?;
    let obj_type = c.u32()?;
    let created = c.timespec()?;
    let modified = c.timespec()?;
    let changed = c.timespec()?;
    let accessed = c.timespec()?;
    let access_mask = c.u32()?;
    let flags = c.u32()?;
    let file_id = c.u64()?;
    let mount_status = if returned.dirattr & libc::ATTR_DIR_MOUNTSTATUS != 0 {
        c.u32()?
    } else {
        0
    };
    let allocated = if returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        c.i64()?
    } else {
        0
    };
    let data_length = if returned.fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
        c.i64()?
    } else {
        0
    };
    Some(ParsedEntry {
        returned,
        error,
        name,
        dev,
        obj_type,
        created,
        modified,
        changed,
        accessed,
        access_mask,
        flags,
        file_id,
        mount_status,
        allocated,
        data_length,
    })
}

fn returned_common(entry: &ParsedEntry<'_>, bit: libc::attrgroup_t) -> bool {
    entry.returned.commonattr & bit != 0
}

fn decode_name(bytes: &[u8]) -> (String, bool) {
    let bytes = match bytes.iter().position(|b| *b == 0) {
        Some(nul) => &bytes[..nul],
        None => bytes,
    };
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

/// Map BSD flags and permission bits onto the portable attribute set. Shared
/// with the portable lister so a directory read through either adapter — a
/// local volume or a fallback readdir on a remote one — describes an object
/// the same way. Only facts macOS actually reports are set: there is no
/// per-file sparse flag, so `SPARSE` is never inferred from a short
/// allocation.
pub(crate) fn attributes_from(
    kind: ObjectKind,
    name: &str,
    mode: Option<u32>,
    flags: Option<u32>,
) -> FileAttributes {
    let mut attributes = FileAttributes::default();
    match kind {
        ObjectKind::Directory => attributes.0 |= FileAttributes::DIRECTORY,
        ObjectKind::Reparse => attributes.0 |= FileAttributes::REPARSE_POINT,
        _ => {}
    }
    if mode.is_some_and(|m| m & 0o222 == 0) {
        attributes.0 |= FileAttributes::READONLY;
    }
    if let Some(flags) = flags {
        if flags & libc::UF_HIDDEN != 0 {
            attributes.0 |= FileAttributes::HIDDEN;
        }
        if flags & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE) != 0 {
            attributes.0 |= FileAttributes::READONLY;
        }
        if flags & libc::UF_COMPRESSED != 0 {
            attributes.0 |= FileAttributes::COMPRESSED;
        }
        // A dataless object is a cloud placeholder: reading it downloads it.
        // The same attributes Windows uses for tiered storage keep the content
        // pipeline from touching the bytes.
        if flags & SF_DATALESS != 0 {
            attributes.0 |= FileAttributes::OFFLINE | FileAttributes::RECALL_ON_DATA_ACCESS;
        }
    }
    // Leading-dot names are hidden by macOS convention rather than by a flag.
    if name.starts_with('.') && name != "." && name != ".." {
        attributes.0 |= FileAttributes::HIDDEN;
    }
    attributes
}

fn attributes_of(entry: &ParsedEntry<'_>, name: &str, kind: ObjectKind) -> FileAttributes {
    attributes_from(
        kind,
        name,
        returned_common(entry, libc::ATTR_CMN_ACCESSMASK).then_some(entry.access_mask),
        returned_common(entry, libc::ATTR_CMN_FLAGS).then_some(entry.flags),
    )
}

/// A mount point is recorded but not descended into, exactly like a Windows
/// volume mount point. Firmlinks stay traversable: not descending them would
/// hide the data volume behind its only user-facing path.
fn is_mount_point(entry: &ParsedEntry<'_>) -> bool {
    entry.returned.dirattr & libc::ATTR_DIR_MOUNTSTATUS != 0
        && entry.mount_status & libc::DIR_MNTSTATUS_MNTPOINT != 0
}

fn to_raw_entry(entry: &ParsedEntry<'_>, confidence: IdentityConfidence) -> RawEntry {
    let (name, name_lossy) = decode_name(entry.name);
    let kind = match entry.obj_type {
        VDIR => ObjectKind::Directory,
        VLNK => ObjectKind::Reparse,
        _ => ObjectKind::File,
    };
    let mut attributes = attributes_of(entry, &name, kind);
    if kind == ObjectKind::Directory && is_mount_point(entry) {
        attributes.0 |= FileAttributes::REPARSE_POINT;
    }
    let is_regular_file = entry.obj_type == VREG;
    let size = if is_regular_file && entry.returned.fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
        entry.data_length.max(0) as u64
    } else {
        0
    };
    let allocated = if is_regular_file && entry.returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        Some(entry.allocated.max(0) as u64)
    } else {
        None
    };
    let time = |bit: libc::attrgroup_t, value: UnixNanos| -> Option<UnixNanos> {
        returned_common(entry, bit).then_some(value)
    };
    let native_id = returned_common(entry, libc::ATTR_CMN_FILEID).then_some(NativeIdentity {
        // `dev_t` is a signed 32-bit value; widen it exactly the way
        // `MetadataExt::dev` does so both listers report one identity.
        volume_serial: entry.dev as i64 as u64,
        file_id_high: 0,
        file_id_low: entry.file_id,
        confidence,
    });
    RawEntry {
        name,
        name_lossy,
        kind,
        attributes,
        size,
        allocated,
        created: time(libc::ATTR_CMN_CRTIME, entry.created),
        modified: time(libc::ATTR_CMN_MODTIME, entry.modified),
        changed: time(libc::ATTR_CMN_CHGTIME, entry.changed),
        accessed: time(libc::ATTR_CMN_ACCTIME, entry.accessed),
        native_id,
        reparse_tag: 0,
    }
}

/// Capabilities of one mounted volume, as the kernel reports them.
struct VolumeFacts {
    capabilities: [u32; 4],
    valid: [u32; 4],
    min_allocation: u64,
    name: String,
}

fn volume_facts(mount_point: &Path) -> Option<VolumeFacts> {
    let path = cstring(mount_point).ok()?;
    let mut list = attr_list(libc::ATTR_CMN_RETURNED_ATTRS, VOL_ATTRS, 0, 0);
    let mut buf = [0u8; 1024];
    // SAFETY: `list` and `buf` are live, correctly sized, and owned here; the
    // call only reads `path`.
    let rc = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            &mut list as *mut libc::attrlist as *mut libc::c_void,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::FSOPT_PACK_INVAL_ATTRS,
        )
    };
    if rc != 0 {
        return None;
    }
    let length = u32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let group = buf.get(..length.min(buf.len()))?;
    let mut c = AttrCursor::new(group);
    let _length = c.u32()?;
    let returned = c.attribute_set()?;
    let min_allocation = c.i64()?;
    let name_bytes = c.attr_reference().unwrap_or(&[]);
    let mut capabilities = [0u32; 4];
    let mut valid = [0u32; 4];
    for slot in capabilities.iter_mut() {
        *slot = c.u32()?;
    }
    for slot in valid.iter_mut() {
        *slot = c.u32()?;
    }
    if returned.volattr & libc::ATTR_VOL_CAPABILITIES == 0 {
        valid = [0u32; 4];
    }
    Some(VolumeFacts {
        capabilities,
        valid,
        min_allocation: if returned.volattr & libc::ATTR_VOL_MINALLOCATION != 0 {
            min_allocation.max(0) as u64
        } else {
            0
        },
        name: decode_name(name_bytes).0,
    })
}

impl VolumeFacts {
    /// A capability is only a fact when the volume says the bit is meaningful.
    fn format(&self, bit: u32) -> Option<bool> {
        let index = libc::VOL_CAPABILITIES_FORMAT;
        (self.valid[index] & bit != 0).then_some(self.capabilities[index] & bit != 0)
    }
}

fn cstr_to_string(bytes: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = bytes
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `st_dev` for `path`, the value the portable lister reports per entry.
fn device_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0)
}

fn statfs_of(path: &Path) -> Result<libc::statfs, ScanError> {
    let c_path = cstring(path)?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `fs` is a live, owned buffer.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut fs) } != 0 {
        return Err(last_error(path));
    }
    Ok(fs)
}

/// macOS enumeration adapter. Falls back to [`StdLister`] whenever the bulk
/// path is not applicable or not trustworthy for a directory.
pub struct MacLister {
    fallback: StdLister,
}

impl MacLister {
    pub fn new() -> Self {
        Self {
            fallback: StdLister,
        }
    }

    /// Whether `dir` is on a locally mounted volume. The bulk path is only
    /// used there (see the module comment on FB15497909).
    fn is_local(dir: &Path) -> bool {
        statfs_of(dir).is_ok_and(|fs| fs.f_flags & libc::MNT_LOCAL as u32 != 0)
    }

    fn list_bulk(&self, dir: &Path) -> Result<Vec<RawEntry>, ScanError> {
        let c_dir = cstring(dir)?;
        // SAFETY: `c_dir` is NUL-terminated; the descriptor is closed below.
        let fd = unsafe { libc::open(c_dir.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(last_error(dir));
        }
        let guard = FdGuard(fd);
        let mut list = attr_list(COMMON_ATTRS, 0, DIR_ATTRS, FILE_ATTRS);
        let mut buf = vec![0u8; ATTR_BUF_LEN];
        let mut out = Vec::new();
        loop {
            // SAFETY: the descriptor is open for reading, `list` describes the
            // requested attributes, and `buf` is a live buffer of `buf.len()`.
            let count = unsafe {
                libc::getattrlistbulk(
                    guard.0,
                    &mut list as *mut libc::attrlist as *mut libc::c_void,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    BULK_OPTIONS,
                )
            };
            if count < 0 {
                return Err(last_error(dir));
            }
            if count == 0 {
                return Ok(out);
            }
            let mut offset = 0usize;
            for _ in 0..count {
                let remaining = buf.get(offset..).ok_or_else(|| unsupported(dir))?;
                if remaining.len() < 4 {
                    return Err(unsupported(dir));
                }
                let length = u32::from_ne_bytes(remaining[..4].try_into().unwrap()) as usize;
                if length < 4 || length > remaining.len() {
                    return Err(unsupported(dir));
                }
                let group = &remaining[..length];
                let entry = parse_group(group).ok_or_else(|| unsupported(dir))?;
                offset += length;
                if entry.error != 0 {
                    // A per-entry error is the kernel telling us this child
                    // could not be read; the rest of the batch is still valid.
                    tracing::debug!(
                        dir = %dir.display(),
                        error = entry.error,
                        "getattrlistbulk reported a per-entry error"
                    );
                    continue;
                }
                if entry.returned.commonattr & libc::ATTR_CMN_NAME == 0 {
                    return Err(unsupported(dir));
                }
                let (name, _) = decode_name(entry.name);
                if name.is_empty() || name == "." || name == ".." {
                    continue;
                }
                out.push(to_raw_entry(&entry, IDENTITY_CONFIDENCE));
            }
        }
    }
}

/// macOS inode numbers survive renames within a volume, but reuse after
/// deletion, volume restores, and clone semantics have not been measured on
/// this project's corpus yet, so the identity is deliberately not claimed to
/// be stable across runs.
const IDENTITY_CONFIDENCE: IdentityConfidence = IdentityConfidence::Weak;

fn unsupported(dir: &Path) -> ScanError {
    ScanError::new(
        ScanErrorKind::Unsupported,
        0,
        "getattrlistbulk returned an unexpected attribute layout",
        dir,
    )
}

struct FdGuard(libc::c_int);

impl Drop for FdGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor was opened by `list_bulk` and is closed once.
        unsafe { libc::close(self.0) };
    }
}

impl Default for MacLister {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectoryLister for MacLister {
    fn list(&self, dir: &Path) -> Result<Vec<RawEntry>, ScanError> {
        if !Self::is_local(dir) {
            return self.fallback.list(dir);
        }
        match self.list_bulk(dir) {
            Ok(entries) => Ok(entries),
            Err(e) if e.kind == ScanErrorKind::Unsupported => {
                tracing::debug!(dir = %dir.display(), error = %e, "falling back to readdir");
                self.fallback.list(dir)
            }
            Err(e) => Err(e),
        }
    }

    fn volume_info(&self, root: &Path) -> Result<VolumeInfo, ScanError> {
        let fs = statfs_of(root)?;
        let filesystem = cstr_to_string(&fs.f_fstypename);
        let volume_root = cstr_to_string(&fs.f_mntonname);
        let local = fs.f_flags & libc::MNT_LOCAL as u32 != 0;
        let drive_type = if !local {
            DriveType::Remote
        } else if fs.f_flags & MNT_REMOVABLE != 0 {
            DriveType::Removable
        } else {
            DriveType::Fixed
        };
        let facts = volume_facts(Path::new(&volume_root));
        let capability = |bit: u32| facts.as_ref().and_then(|f| f.format(bit));
        let volume_name = facts
            .as_ref()
            .map(|f| f.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                Path::new(&volume_root)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| volume_root.clone())
            });
        let bytes_per_cluster = facts
            .as_ref()
            .map(|f| f.min_allocation)
            .filter(|v| *v > 0)
            .unwrap_or(fs.f_bsize as u64);
        // FSEvents is served per mounted volume by `fseventsd`; the stream is
        // only opened for locally mounted volumes, and the adapter confirms
        // the stream really started before a source relies on it.
        let native_feed = if local && matches!(filesystem.as_str(), "apfs" | "hfs") {
            NativeFeed::MacosFsEvents
        } else {
            NativeFeed::None
        };
        Ok(VolumeInfo {
            // The same `st_dev` both listers report per entry, so a volume row
            // and the objects on it agree on one serial.
            volume_serial: device_of(root),
            filesystem,
            volume_name,
            fs_flags: fs.f_flags,
            drive_type,
            supports_file_ids: capability(libc::VOL_CAP_FMT_PERSISTENTOBJECTIDS).unwrap_or(false)
                || capability(libc::VOL_CAP_FMT_64BIT_OBJECT_IDS).unwrap_or(false),
            supports_usn: false,
            supports_hard_links: capability(libc::VOL_CAP_FMT_HARDLINKS).unwrap_or(false),
            supports_reparse_points: capability(libc::VOL_CAP_FMT_SYMBOLICLINKS).unwrap_or(false),
            supports_sparse: capability(libc::VOL_CAP_FMT_SPARSE_FILES).unwrap_or(false),
            case_sensitive: capability(libc::VOL_CAP_FMT_CASE_SENSITIVE),
            native_feed,
            bytes_per_cluster: bytes_per_cluster.min(u32::MAX as u64) as u32,
            volume_root,
        })
    }

    fn stat(&self, path: &Path) -> Result<RawEntry, ScanError> {
        let c_path = cstring(path)?;
        let mut list = attr_list(COMMON_ATTRS, 0, DIR_ATTRS, FILE_ATTRS);
        let mut buf = vec![0u8; 4096];
        // SAFETY: `c_path` is NUL-terminated and `buf` is a live owned buffer.
        let rc = unsafe {
            libc::getattrlist(
                c_path.as_ptr(),
                &mut list as *mut libc::attrlist as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(last_error(path));
        }
        let length = u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
        if length < 4 || length > buf.len() {
            return self.fallback.stat(path);
        }
        match parse_group(&buf[..length]) {
            Some(entry) if entry.error == 0 => Ok(to_raw_entry(&entry, IDENTITY_CONFIDENCE)),
            _ => self.fallback.stat(path),
        }
    }

    fn name(&self) -> &'static str {
        "macos-getattrlistbulk"
    }
}

/// Canonical user-facing form of a macOS root: an absolute path without a
/// trailing separator. Unicode is left exactly as the filesystem returned it;
/// HFS+ normalises names to NFD and APFS does not, and rewriting either would
/// make a path stop matching the volume it came from.
pub fn normalize_root(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_lose_their_trailing_separator() {
        assert_eq!(normalize_root("/Volumes/Corpus/"), "/Volumes/Corpus");
        assert_eq!(normalize_root("/"), "/");
        assert_eq!(normalize_root("//"), "/");
    }

    #[test]
    fn names_stop_at_the_first_nul() {
        let (name, lossy) = decode_name(b"alpha\0\0\0");
        assert_eq!(name, "alpha");
        assert!(!lossy);
    }

    #[test]
    fn invalid_utf8_names_are_flagged() {
        let (name, lossy) = decode_name(&[0x66, 0xff, 0x00]);
        assert!(lossy, "{name} should be reported as lossy");
    }

    #[test]
    fn a_truncated_group_is_rejected_instead_of_guessed() {
        assert!(parse_group(&[0u8; 8]).is_none());
    }
}
