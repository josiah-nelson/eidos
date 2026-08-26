//! NTFS/ReFS USN change journal access and by-ID file snapshots.
//!
//! The journal is read with `FSCTL_READ_USN_JOURNAL` from a volume handle.
//! Records are normalised into [`UsnRecord`] (128-bit reference numbers for
//! both NTFS V2 and ReFS V3 layouts) so higher layers never see the raw
//! structures. Overflow (`ERROR_JOURNAL_ENTRY_DELETED`) and journal identity
//! changes are first-class outcomes, not errors.
//!
//! Reading the journal requires an elevated process (or at least
//! `SE_MANAGE_VOLUME`/backup rights); callers must treat
//! [`UsnError::AccessDenied`] as "fall back to periodic reconciliation".

use crate::error::{classify_os_error, ScanError, ScanErrorKind};
use crate::win::{to_wide_extended, Handle};
use eidos_domain::{FileAttributes, IdentityConfidence, NativeIdentity, ObjectKind, UnixNanos};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF,
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING,
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_NOT_FOUND, ERROR_NO_MORE_FILES, GENERIC_READ, INVALID_HANDLE_VALUE, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ExtendedFileIdType, FileAttributeTagInfo, FileBasicInfo, FileIdInfo,
    FileStandardInfo, FindClose, FindFirstFileNameW, FindNextFileNameW,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, OpenFileById, FILE_ATTRIBUTE_TAG_INFO,
    FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_OVERLAPPED, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2,
    USN_RECORD_V2, USN_RECORD_V3,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};

pub use windows_sys::Win32::System::Ioctl::{
    USN_REASON_BASIC_INFO_CHANGE, USN_REASON_CLOSE, USN_REASON_DATA_EXTEND,
    USN_REASON_DATA_OVERWRITE, USN_REASON_DATA_TRUNCATION, USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME, USN_REASON_REPARSE_POINT_CHANGE, USN_REASON_STREAM_CHANGE,
};

#[derive(Debug, thiserror::Error)]
pub enum UsnError {
    #[error("USN journal is not active on this volume")]
    NotActive,
    #[error("USN journal deletion in progress")]
    DeleteInProgress,
    #[error("access denied reading the USN journal (elevation required)")]
    AccessDenied,
    #[error("USN journal not supported on this volume")]
    Unsupported,
    #[error("USN journal read cancelled")]
    Cancelled,
    #[error("{0}")]
    Io(ScanError),
}

/// Open handle to a volume (`\\.\G:`).
pub struct VolumeHandle {
    handle: Handle,
    pub root: String,
}

impl VolumeHandle {
    /// `volume_root` is the drive root such as `G:\`.
    pub fn open(volume_root: &str) -> Result<VolumeHandle, UsnError> {
        Self::open_with_flags(volume_root, 0)
    }

    /// Open a volume for an overlapped journal read that can wait on a
    /// cancellation event as well as journal activity.
    pub fn open_waitable(volume_root: &str) -> Result<VolumeHandle, UsnError> {
        Self::open_with_flags(volume_root, FILE_FLAG_OVERLAPPED)
    }

    /// Open a volume by its device path, e.g. `\\?\Volume{guid}` (no
    /// trailing separator), for volumes that have no drive letter.
    pub fn open_device(device: &str, waitable: bool) -> Result<VolumeHandle, UsnError> {
        let flags = if waitable { FILE_FLAG_OVERLAPPED } else { 0 };
        Self::open_device_with_flags(device, device, flags)
    }

    fn open_with_flags(volume_root: &str, flags: u32) -> Result<VolumeHandle, UsnError> {
        let letter = volume_root.trim_end_matches('\\');
        let device = format!("\\\\.\\{letter}");
        Self::open_device_with_flags(volume_root, &device, flags)
    }

    fn open_device_with_flags(
        volume_root: &str,
        device: &str,
        flags: u32,
    ) -> Result<VolumeHandle, UsnError> {
        let mut wide: Vec<u16> = std::ffi::OsStr::new(&device).encode_wide().collect();
        wide.push(0);
        // SAFETY: NUL-terminated path; no security attributes.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: trivially safe.
            let code = unsafe { GetLastError() };
            return Err(match code {
                ERROR_ACCESS_DENIED => UsnError::AccessDenied,
                _ => UsnError::Io(ScanError::new(
                    classify_os_error(code as i32, std::io::ErrorKind::Other),
                    code as i32,
                    format!(
                        "CreateFileW({device}): {}",
                        std::io::Error::from_raw_os_error(code as i32)
                    ),
                    Path::new(volume_root),
                )),
            });
        }
        Ok(VolumeHandle {
            handle: Handle(h),
            root: volume_root.to_string(),
        })
    }

    pub fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle.0
    }
}

/// Manual-reset event used to interrupt an overlapped journal read.
pub struct JournalCancellation {
    event: Handle,
    cancelled: std::sync::atomic::AtomicBool,
}

// SAFETY: kernel event handles are designed for cross-thread signaling. The
// owned handle remains alive for every waiter through the shared status value.
unsafe impl Send for JournalCancellation {}
unsafe impl Sync for JournalCancellation {}

impl JournalCancellation {
    pub fn new() -> std::io::Result<Self> {
        // SAFETY: default security, manual reset, initially nonsignaled, no name.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self {
                event: Handle(event),
                cancelled: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        // SAFETY: `event` is a live manual-reset event.
        unsafe {
            SetEvent(self.event.0);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
    pub min_major_version: u16,
    pub max_major_version: u16,
}

fn usn_error(code: u32, what: &str, root: &str) -> UsnError {
    match code {
        ERROR_JOURNAL_NOT_ACTIVE => UsnError::NotActive,
        ERROR_JOURNAL_DELETE_IN_PROGRESS => UsnError::DeleteInProgress,
        ERROR_ACCESS_DENIED => UsnError::AccessDenied,
        ERROR_INVALID_FUNCTION => UsnError::Unsupported,
        _ => UsnError::Io(ScanError::new(
            classify_os_error(code as i32, std::io::ErrorKind::Other),
            code as i32,
            format!("{what}: {}", std::io::Error::from_raw_os_error(code as i32)),
            Path::new(root),
        )),
    }
}

/// `FSCTL_QUERY_USN_JOURNAL`.
pub fn query_journal(vol: &VolumeHandle) -> Result<JournalInfo, UsnError> {
    let mut data = USN_JOURNAL_DATA_V2 {
        UsnJournalID: 0,
        FirstUsn: 0,
        NextUsn: 0,
        LowestValidUsn: 0,
        MaxUsn: 0,
        MaximumSize: 0,
        AllocationDelta: 0,
        MinSupportedMajorVersion: 0,
        MaxSupportedMajorVersion: 0,
        Flags: 0,
        RangeTrackChunkSize: 0,
        RangeTrackFileSizeThreshold: 0,
    };
    let mut returned: u32 = 0;
    // SAFETY: output buffer is the V2 struct; the driver may fill a V0/V1
    // prefix on older systems, which is layout-compatible.
    let ok = unsafe {
        DeviceIoControl(
            vol.raw(),
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut data as *mut _ as *mut _,
            std::mem::size_of::<USN_JOURNAL_DATA_V2>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: trivially safe.
        let code = unsafe { GetLastError() };
        return Err(usn_error(code, "FSCTL_QUERY_USN_JOURNAL", &vol.root));
    }
    Ok(JournalInfo {
        journal_id: data.UsnJournalID,
        first_usn: data.FirstUsn,
        next_usn: data.NextUsn,
        lowest_valid_usn: data.LowestValidUsn,
        max_usn: data.MaxUsn,
        maximum_size: data.MaximumSize,
        allocation_delta: data.AllocationDelta,
        min_major_version: data.MinSupportedMajorVersion,
        max_major_version: data.MaxSupportedMajorVersion,
    })
}

/// One normalised journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub usn: i64,
    /// 128-bit file reference (NTFS 64-bit FRNs are zero-extended).
    pub frn: u128,
    pub parent_frn: u128,
    pub reason: u32,
    pub attributes: FileAttributes,
    pub name: String,
    pub timestamp: UnixNanos,
    pub version: u16,
}

impl UsnRecord {
    pub fn is_directory(&self) -> bool {
        self.attributes.is_directory()
    }
    pub fn has(&self, reason: u32) -> bool {
        self.reason & reason != 0
    }
}

/// Result of one journal read.
#[derive(Debug)]
pub enum ReadOutcome {
    /// Zero or more records; `next_usn` is the checkpoint to store once the
    /// records are durably applied.
    Records {
        records: Vec<UsnRecord>,
        next_usn: i64,
    },
    /// The requested start USN is no longer in the journal (overflow).
    EntryDeleted,
    /// The journal was deleted/recreated; the stored journal ID is invalid.
    JournalChanged,
}

/// Read records from `start_usn`. `buf` should be at least 64 KiB.
pub fn read_journal(
    vol: &VolumeHandle,
    journal_id: u64,
    start_usn: i64,
    buf: &mut [u8],
) -> Result<ReadOutcome, UsnError> {
    read_journal_opts(vol, journal_id, start_usn, buf, None)
}

/// Like [`read_journal`], but when the journal is drained the call remains
/// blocked in the kernel until at least one new record arrives or another
/// thread signals `cancel`. This is the push-style
/// feed: change latency becomes the ioctl wake-up rather than a poll interval.
pub fn read_journal_wait(
    vol: &VolumeHandle,
    journal_id: u64,
    start_usn: i64,
    buf: &mut [u8],
    cancel: &JournalCancellation,
) -> Result<ReadOutcome, UsnError> {
    if cancel.is_cancelled() {
        return Err(UsnError::Cancelled);
    }
    let outcome = read_journal_opts(vol, journal_id, start_usn, buf, Some(cancel))?;
    // DeviceIoControl may complete synchronously, in which case the wait on
    // the cancellation event is bypassed. Never hand that batch to the
    // caller after a concurrent shutdown/remove request has won the race.
    if cancel.is_cancelled() {
        Err(UsnError::Cancelled)
    } else {
        Ok(outcome)
    }
}

fn read_journal_opts(
    vol: &VolumeHandle,
    journal_id: u64,
    start_usn: i64,
    buf: &mut [u8],
    cancel: Option<&JournalCancellation>,
) -> Result<ReadOutcome, UsnError> {
    let req = READ_USN_JOURNAL_DATA_V1 {
        StartUsn: start_usn,
        ReasonMask: 0xFFFF_FFFF,
        ReturnOnlyOnClose: 0,
        // With BytesToWaitFor nonzero, Windows repeats a nonzero Timeout until
        // records exist; it is not a deadline. Use the documented cancellable
        // indefinite mode and let the owner cancel the I/O on shutdown.
        Timeout: 0,
        BytesToWaitFor: u64::from(cancel.is_some()),
        UsnJournalID: journal_id,
        MinMajorVersion: 2,
        MaxMajorVersion: 3,
    };
    let mut returned: u32 = 0;
    let io_event = if cancel.is_some() {
        // SAFETY: default security, manual reset, initially nonsignaled, no name.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(usn_error(
                unsafe { GetLastError() },
                "CreateEventW(journal read)",
                &vol.root,
            ));
        }
        Some(Handle(event))
    } else {
        None
    };
    let mut overlapped = OVERLAPPED::default();
    if let Some(event) = &io_event {
        overlapped.hEvent = event.0;
    }
    // SAFETY: input/output buffers sized as declared; the kernel writes at
    // most `buf.len()` bytes. The event and OVERLAPPED value remain alive
    // until completion or cancellation has itself completed.
    let ok = unsafe {
        DeviceIoControl(
            vol.raw(),
            FSCTL_READ_USN_JOURNAL,
            &req as *const _ as *const _,
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut returned,
            if cancel.is_some() {
                &mut overlapped
            } else {
                std::ptr::null_mut()
            },
        )
    };
    if ok == 0 {
        // SAFETY: trivially safe.
        let code = unsafe { GetLastError() };
        if code == ERROR_IO_PENDING {
            let cancel = cancel.expect("only overlapped reads can be pending");
            let handles = [io_event.as_ref().expect("event").0, cancel.event.0];
            // SAFETY: both handles stay live for this wait; wait for either
            // journal completion or the manual-reset cancellation event.
            match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
                WAIT_OBJECT_0 => {
                    // SAFETY: the I/O event is signaled; retrieve its result.
                    if unsafe { GetOverlappedResult(vol.raw(), &overlapped, &mut returned, 0) } == 0
                    {
                        return read_failure(unsafe { GetLastError() }, start_usn, &vol.root);
                    }
                }
                value if value == WAIT_OBJECT_0 + 1 => {
                    // SAFETY: cancel this exact operation, then wait for its
                    // completion before its buffer and OVERLAPPED are dropped.
                    unsafe {
                        CancelIoEx(vol.raw(), &overlapped);
                        GetOverlappedResult(vol.raw(), &overlapped, &mut returned, 1);
                    }
                    return Err(UsnError::Cancelled);
                }
                WAIT_FAILED => {
                    return Err(usn_error(
                        unsafe { GetLastError() },
                        "WaitForMultipleObjects(journal read)",
                        &vol.root,
                    ));
                }
                _ => unreachable!("two-handle wait returned an invalid index"),
            }
        } else {
            return read_failure(code, start_usn, &vol.root);
        }
    }
    let returned = returned as usize;
    if returned < 8 {
        return Ok(ReadOutcome::Records {
            records: Vec::new(),
            next_usn: start_usn,
        });
    }
    let next_usn = i64::from_le_bytes(buf[0..8].try_into().expect("8 bytes"));
    let mut records = Vec::new();
    let mut off = 8usize;
    while off + 8 <= returned {
        // SAFETY: bounds-checked reads; record headers are 8-byte aligned.
        let (len, rec) = unsafe { parse_record(&buf[..returned], off) }?;
        if len == 0 {
            break;
        }
        if let Some(r) = rec {
            records.push(r);
        }
        off += len;
    }
    Ok(ReadOutcome::Records { records, next_usn })
}

fn read_failure(code: u32, start_usn: i64, root: &str) -> Result<ReadOutcome, UsnError> {
    match code {
        ERROR_JOURNAL_ENTRY_DELETED => Ok(ReadOutcome::EntryDeleted),
        ERROR_INVALID_PARAMETER => Ok(ReadOutcome::JournalChanged),
        ERROR_HANDLE_EOF => Ok(ReadOutcome::Records {
            records: Vec::new(),
            next_usn: start_usn,
        }),
        _ => Err(usn_error(code, "FSCTL_READ_USN_JOURNAL", root)),
    }
}

unsafe fn parse_record(buf: &[u8], off: usize) -> Result<(usize, Option<UsnRecord>), UsnError> {
    let base = buf.as_ptr().add(off);
    let len = u32::from_le_bytes(buf[off..off + 4].try_into().expect("4")) as usize;
    if len < 8 || off + len > buf.len() {
        return Ok((0, None));
    }
    let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().expect("2"));
    let rec = match major {
        2 => {
            if len < std::mem::size_of::<USN_RECORD_V2>() - 2 {
                return Ok((len, None));
            }
            let r = &*(base as *const USN_RECORD_V2);
            let name_off = r.FileNameOffset as usize;
            let name_len = r.FileNameLength as usize;
            if name_off + name_len > len {
                return Ok((len, None));
            }
            let name_ptr = base.add(name_off) as *const u16;
            let units = std::slice::from_raw_parts(name_ptr, name_len / 2);
            Some(UsnRecord {
                usn: r.Usn,
                frn: r.FileReferenceNumber as u128,
                parent_frn: r.ParentFileReferenceNumber as u128,
                reason: r.Reason,
                attributes: FileAttributes(r.FileAttributes),
                name: String::from_utf16_lossy(units),
                timestamp: UnixNanos::from_filetime_ticks(r.TimeStamp),
                version: 2,
            })
        }
        3 => {
            if len < std::mem::size_of::<USN_RECORD_V3>() - 2 {
                return Ok((len, None));
            }
            let r = &*(base as *const USN_RECORD_V3);
            let name_off = r.FileNameOffset as usize;
            let name_len = r.FileNameLength as usize;
            if name_off + name_len > len {
                return Ok((len, None));
            }
            let name_ptr = base.add(name_off) as *const u16;
            let units = std::slice::from_raw_parts(name_ptr, name_len / 2);
            Some(UsnRecord {
                usn: r.Usn,
                frn: u128::from_le_bytes(r.FileReferenceNumber.Identifier),
                parent_frn: u128::from_le_bytes(r.ParentFileReferenceNumber.Identifier),
                reason: r.Reason,
                attributes: FileAttributes(r.FileAttributes),
                name: String::from_utf16_lossy(units),
                timestamp: UnixNanos::from_filetime_ticks(r.TimeStamp),
                version: 3,
            })
        }
        // V4 range-tracking records carry no name; skip.
        _ => None,
    };
    Ok((len, rec))
}

/// Current state of a file opened by reference number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub native: NativeIdentity,
    pub kind: ObjectKind,
    pub attributes: FileAttributes,
    pub size: u64,
    pub allocated: u64,
    pub link_count: u32,
    pub created: Option<UnixNanos>,
    pub modified: Option<UnixNanos>,
    pub changed: Option<UnixNanos>,
    pub accessed: Option<UnixNanos>,
    pub reparse_tag: u32,
    /// Final path (`\\?\G:\...`) as reported by the handle, when available.
    pub path: Option<String>,
}

/// Open a file by its 128-bit reference number. Returns `Ok(None)` if it no
/// longer exists.
pub fn snapshot_by_id(vol: &VolumeHandle, frn: u128) -> Result<Option<FileSnapshot>, ScanError> {
    let mut desc = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: ExtendedFileIdType,
        ..Default::default()
    };
    desc.Anonymous.ExtendedFileId = FILE_ID_128 {
        Identifier: frn.to_le_bytes(),
    };
    // SAFETY: descriptor fully initialised; volume handle valid.
    let h = unsafe {
        OpenFileById(
            vol.raw(),
            &desc,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe.
        let code = unsafe { GetLastError() };
        return match code {
            ERROR_FILE_NOT_FOUND | ERROR_INVALID_PARAMETER | ERROR_NOT_FOUND => Ok(None),
            _ => Err(ScanError::new(
                classify_os_error(code as i32, std::io::ErrorKind::Other),
                code as i32,
                format!(
                    "OpenFileById: {}",
                    std::io::Error::from_raw_os_error(code as i32)
                ),
                Path::new(&vol.root),
            )),
        };
    }
    let h = Handle(h);
    snapshot_from_handle(&h, Path::new(&vol.root)).map(Some)
}

/// Snapshot an existing path (used for the source root and tests).
pub fn snapshot_path(path: &Path) -> Result<FileSnapshot, ScanError> {
    let wide = to_wide_extended(path);
    // SAFETY: NUL-terminated path.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe.
        let code = unsafe { GetLastError() } as i32;
        return Err(ScanError::from_io(
            path,
            &std::io::Error::from_raw_os_error(code),
        ));
    }
    snapshot_from_handle(&Handle(h), path)
}

fn snapshot_from_handle(h: &Handle, ctx: &Path) -> Result<FileSnapshot, ScanError> {
    let fail = |what: &str| {
        // SAFETY: trivially safe.
        let code = unsafe { GetLastError() } as i32;
        ScanError::new(
            classify_os_error(code, std::io::ErrorKind::Other),
            code,
            format!("{what}: {}", std::io::Error::from_raw_os_error(code)),
            ctx,
        )
    };
    let mut id = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO {
        CreationTime: 0,
        LastAccessTime: 0,
        LastWriteTime: 0,
        ChangeTime: 0,
        FileAttributes: 0,
    };
    let mut std_info = FILE_STANDARD_INFO {
        AllocationSize: 0,
        EndOfFile: 0,
        NumberOfLinks: 0,
        DeletePending: false,
        Directory: false,
    };
    let mut tag = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: each out struct sized as declared.
    unsafe {
        if GetFileInformationByHandleEx(
            h.0,
            FileIdInfo,
            &mut id as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) == 0
        {
            return Err(fail("FileIdInfo"));
        }
        if GetFileInformationByHandleEx(
            h.0,
            FileBasicInfo,
            &mut basic as *mut _ as *mut _,
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        ) == 0
        {
            return Err(fail("FileBasicInfo"));
        }
        if GetFileInformationByHandleEx(
            h.0,
            FileStandardInfo,
            &mut std_info as *mut _ as *mut _,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        ) == 0
        {
            return Err(fail("FileStandardInfo"));
        }
        // Reparse tag is best-effort.
        let _ = GetFileInformationByHandleEx(
            h.0,
            FileAttributeTagInfo,
            &mut tag as *mut _ as *mut _,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        );
    }
    let attributes = FileAttributes(basic.FileAttributes);
    let kind = crate::win::object_kind(attributes);
    let ft = |v: i64| {
        if v == 0 {
            None
        } else {
            Some(UnixNanos::from_filetime_ticks(v))
        }
    };
    let mut path_buf = vec![0u16; 1024];
    // SAFETY: buffer sized as declared.
    let n =
        unsafe { GetFinalPathNameByHandleW(h.0, path_buf.as_mut_ptr(), path_buf.len() as u32, 0) }
            as usize;
    let path = if n > 0 && n < path_buf.len() {
        Some(String::from_utf16_lossy(&path_buf[..n]))
    } else {
        None
    };
    Ok(FileSnapshot {
        native: NativeIdentity::from_u128(
            id.VolumeSerialNumber,
            u128::from_le_bytes(id.FileId.Identifier),
            IdentityConfidence::Native,
        ),
        kind,
        attributes,
        size: if kind == ObjectKind::File {
            std_info.EndOfFile.max(0) as u64
        } else {
            0
        },
        allocated: std_info.AllocationSize.max(0) as u64,
        link_count: std_info.NumberOfLinks,
        created: ft(basic.CreationTime),
        modified: ft(basic.LastWriteTime),
        changed: ft(basic.ChangeTime),
        accessed: ft(basic.LastAccessTime),
        reparse_tag: if attributes.is_reparse() {
            tag.ReparseTag
        } else {
            0
        },
        path,
    })
}

/// All hard-link names of a file, as volume-relative paths (`\dir\name`).
pub fn hard_link_names(path: &Path) -> Result<Vec<String>, ScanError> {
    let wide = to_wide_extended(path);
    let mut out = Vec::new();
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buffers sized as declared.
    let h = unsafe { FindFirstFileNameW(wide.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe.
        let code = unsafe { GetLastError() } as i32;
        return Err(ScanError::from_io(
            path,
            &std::io::Error::from_raw_os_error(code),
        ));
    }
    let push = |buf: &[u16], len: u32, out: &mut Vec<String>| {
        let n = (len as usize).saturating_sub(1).min(buf.len());
        out.push(String::from_utf16_lossy(&buf[..n]));
    };
    push(&buf, len, &mut out);
    loop {
        len = buf.len() as u32;
        // SAFETY: handle valid until FindClose.
        let ok = unsafe { FindNextFileNameW(h, &mut len, buf.as_mut_ptr()) };
        if ok == 0 {
            // SAFETY: trivially safe.
            let code = unsafe { GetLastError() };
            // SAFETY: handle valid.
            unsafe { FindClose(h) };
            // FindNextFileNameW reports the end of the link list as
            // ERROR_HANDLE_EOF; be lenient and also accept NO_MORE_FILES.
            if code == ERROR_HANDLE_EOF || code == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(ScanError::new(
                classify_os_error(code as i32, std::io::ErrorKind::Other),
                code as i32,
                "FindNextFileNameW",
                path,
            ));
        }
        push(&buf, len, &mut out);
    }
    Ok(out)
}

/// Convert a `ScanError` kind into whether the journal-backed watcher should
/// treat the failure as transient.
pub fn is_transient(e: &ScanError) -> bool {
    matches!(e.kind, ScanErrorKind::Transient)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_volume_root() -> Option<String> {
        let tmp = std::env::temp_dir();
        let s = tmp.to_string_lossy();
        s.get(0..3).map(|r| r.to_string())
    }

    #[test]
    fn snapshot_of_temp_file_has_identity() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.txt");
        std::fs::write(&p, b"hello").unwrap();
        let s = snapshot_path(&p).unwrap();
        assert_eq!(s.kind, ObjectKind::File);
        assert_eq!(s.size, 5);
        assert_eq!(s.link_count, 1);
        assert!(s.native.volume_serial != 0);
        assert!(s.path.as_deref().unwrap_or("").ends_with("x.txt"));
        let d = snapshot_path(dir.path()).unwrap();
        assert_eq!(d.kind, ObjectKind::Directory);
    }

    #[test]
    fn hard_links_enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::hard_link(&a, dir.path().join("b.txt")).unwrap();
        let names = hard_link_names(&a).unwrap();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| n.starts_with('\\')));
    }

    #[test]
    fn journal_query_and_read_or_skip() {
        let root = match temp_volume_root() {
            Some(r) => r,
            None => return,
        };
        let vol = match VolumeHandle::open(&root) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping USN test: {e}");
                return;
            }
        };
        let info = match query_journal(&vol) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("skipping USN test: {e}");
                return;
            }
        };
        assert!(info.next_usn >= info.first_usn);
        // Create a file and confirm a record appears after the checkpoint.
        let dir = tempfile::tempdir().unwrap();
        let marker = format!("eidos-usn-{}.txt", std::process::id());
        std::fs::write(dir.path().join(&marker), b"1").unwrap();
        let mut buf = vec![0u8; 256 * 1024];
        let mut usn = info.next_usn;
        let mut found = false;
        for _ in 0..50 {
            match read_journal(&vol, info.journal_id, usn, &mut buf).unwrap() {
                ReadOutcome::Records { records, next_usn } => {
                    if records
                        .iter()
                        .any(|r| r.name == marker && r.has(USN_REASON_FILE_CREATE))
                    {
                        found = true;
                        break;
                    }
                    if next_usn == usn {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    usn = next_usn;
                }
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        assert!(found, "created file did not appear in the journal");
        // By-id snapshot round trip.
        let snap = snapshot_path(&dir.path().join(&marker)).unwrap();
        let again = snapshot_by_id(&vol, snap.native.file_id_u128())
            .unwrap()
            .unwrap();
        assert_eq!(again.native, snap.native);
        std::fs::remove_file(dir.path().join(&marker)).unwrap();
        assert!(snapshot_by_id(&vol, snap.native.file_id_u128())
            .unwrap()
            .is_none());
        // Overflow simulation: a USN far below the first valid one.
        match read_journal(&vol, info.journal_id, 0, &mut buf).unwrap() {
            ReadOutcome::EntryDeleted | ReadOutcome::Records { .. } => {}
            ReadOutcome::JournalChanged => panic!("journal id should still be valid"),
        }
        // Wrong journal id → JournalChanged.
        match read_journal(&vol, info.journal_id ^ 0x5555, info.next_usn, &mut buf).unwrap() {
            ReadOutcome::JournalChanged => {}
            other => panic!("expected JournalChanged, got {other:?}"),
        }
    }

    #[test]
    fn waiting_read_returns_promptly_when_records_arrive_or_skip() {
        let root = match temp_volume_root() {
            Some(r) => r,
            None => return,
        };
        let query_volume = match VolumeHandle::open(&root) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping USN test: {e}");
                return;
            }
        };
        let info = match query_journal(&query_volume) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("skipping USN test: {e}");
                return;
            }
        };
        let vol = VolumeHandle::open_waitable(&root).unwrap();
        let cancel = JournalCancellation::new().unwrap();
        // A writer that fires while the waiting read is blocked in the
        // kernel. The volume is shared, so any process's records also wake
        // the read — the assertion is only that it returns promptly with
        // records rather than sleeping out the full window.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir
            .path()
            .join(format!("eidos-wait-{}.txt", std::process::id()));
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            std::fs::write(&marker, b"wake").unwrap();
        });
        let mut buf = vec![0u8; 256 * 1024];
        let started = std::time::Instant::now();
        let outcome =
            read_journal_wait(&vol, info.journal_id, info.next_usn, &mut buf, &cancel).unwrap();
        writer.join().unwrap();
        match outcome {
            ReadOutcome::Records { records, next_usn } => {
                // Woken by records (ours or a bystander's), not a timer.
                assert!(
                    !records.is_empty() || next_usn != info.next_usn,
                    "waiting read returned empty without advancing"
                );
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(9),
                    "waiting read slept out its window despite volume activity"
                );
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    #[test]
    fn presignalled_cancellation_prevents_a_waiting_read() {
        let root = match temp_volume_root() {
            Some(r) => r,
            None => return,
        };
        let query_volume = match VolumeHandle::open(&root) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping USN test: {e}");
                return;
            }
        };
        let info = match query_journal(&query_volume) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("skipping USN test: {e}");
                return;
            }
        };
        let vol = VolumeHandle::open_waitable(&root).unwrap();
        let cancel = JournalCancellation::new().unwrap();
        cancel.cancel();
        let mut buf = vec![0u8; 256 * 1024];
        let started = std::time::Instant::now();
        let outcome = read_journal_wait(&vol, info.journal_id, info.next_usn, &mut buf, &cancel);
        assert!(matches!(outcome, Err(UsnError::Cancelled)));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }
}
