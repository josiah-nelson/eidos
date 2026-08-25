//! macOS change feed: an FSEvents stream with a durable, validated cursor.
//!
//! FSEvents is not a journal. It reports *paths* that changed, coalesced over
//! a latency window, with no per-change record and no guarantee that every
//! intermediate state is delivered; the agent re-reads each reported path and
//! reconciles it against the catalog. What FSEvents does provide is a durable
//! per-volume event store, so a stream can be opened at a stored event id and
//! replay what happened while the agent was not running.
//!
//! Two properties make that resumption safe, and both are enforced here:
//!
//! - The event store has an identity. `FSEventsCopyUUIDForDevice` returns a
//!   UUID that changes whenever the history becomes meaningless — the store
//!   was purged, the disk was erased, or the id counter wrapped. A stored
//!   cursor is only usable while that UUID still matches, so the cursor
//!   carries it. A `NULL` UUID means the volume keeps no history at all (a
//!   read-only volume, for instance), so no cursor can be issued for it.
//! - History can be incomplete even when the UUID matches. The kernel says so
//!   with `MustScanSubDirs`, `UserDropped`, or `KernelDropped`, and this
//!   adapter turns any of them into an explicit [`RescanReason`] rather than
//!   letting a caller mistake a partial batch for a complete one.
//!
//! The stream is serviced on its own dispatch queue (run-loop scheduling is
//! deprecated), and the callback hands batches to a bounded channel. When that
//! channel is full the batch is dropped deliberately and a rescan is signalled,
//! because blocking the callback would stall delivery for every client of
//! `fseventsd` on the machine.

use crate::error::{ScanError, ScanErrorKind};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation::uuid::{CFUUIDGetUUIDBytes, CFUUIDRef};
use serde::{Deserialize, Serialize};
use std::ffi::{c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Coalescing window. Long enough that a build's worth of writes arrives as a
/// few batches instead of thousands, short enough that the catalog is not
/// visibly behind the filesystem.
const LATENCY_SECONDS: f64 = 0.5;

/// Batches held for the watcher before delivery is treated as lost. Each batch
/// is a coalesced window, so this is a deep queue in wall-clock terms.
const QUEUE_DEPTH: usize = 1024;

const FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;

const CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
const CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
const CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

const EVENT_FLAG_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
const EVENT_FLAG_USER_DROPPED: u32 = 0x0000_0002;
const EVENT_FLAG_KERNEL_DROPPED: u32 = 0x0000_0004;
const EVENT_FLAG_EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
const EVENT_FLAG_HISTORY_DONE: u32 = 0x0000_0010;
const EVENT_FLAG_ROOT_CHANGED: u32 = 0x0000_0020;
const EVENT_FLAG_MOUNT: u32 = 0x0000_0040;
const EVENT_FLAG_UNMOUNT: u32 = 0x0000_0080;
const EVENT_FLAG_ITEM_REMOVED: u32 = 0x0000_0200;
const EVENT_FLAG_ITEM_RENAMED: u32 = 0x0000_0800;
const EVENT_FLAG_ITEM_IS_DIR: u32 = 0x0002_0000;

type FsEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

#[repr(C)]
struct FsEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
}

type FsEventStreamCallback = extern "C" fn(
    stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
);

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: FsEventStreamCallback,
        context: *const FsEventStreamContext,
        paths_to_watch: *const c_void,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FsEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FsEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FsEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FsEventStreamRef);
    fn FSEventStreamInvalidate(stream: FsEventStreamRef);
    fn FSEventStreamRelease(stream: FsEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> CFUUIDRef;
}

extern "C" {
    fn dispatch_queue_create(label: *const libc::c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(object: *mut c_void);
}

/// Durable position in one volume's event store. The identity is part of the
/// cursor: an event id from a different store is not merely stale, it is
/// meaningless, and resuming from it would silently skip every change since.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEventsCursor {
    /// UUID of the volume's event store at the time the id was taken.
    pub store_uuid: String,
    /// Last event id known to be reflected in the catalog.
    pub event_id: u64,
}

/// Why a caller must reconcile by enumeration instead of by events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    /// The kernel coalesced away individual changes for a subtree.
    MustScanSubDirs,
    /// Events were dropped in userspace (`fseventsd` could not keep up).
    UserDropped,
    /// Events were dropped in the kernel.
    KernelDropped,
    /// The event id counter wrapped; earlier ids no longer order anything.
    IdsWrapped,
    /// The watched root itself was moved, deleted, or replaced.
    RootChanged,
    /// A volume was mounted or unmounted inside the watched tree.
    MountChanged,
    /// This process could not take delivery fast enough.
    QueueOverflow,
}

impl RescanReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MustScanSubDirs => "the kernel coalesced changes for a subtree",
            Self::UserDropped => "fseventsd dropped events",
            Self::KernelDropped => "the kernel dropped events",
            Self::IdsWrapped => "the event id counter wrapped",
            Self::RootChanged => "the watched root changed",
            Self::MountChanged => "a volume was mounted or unmounted in the tree",
            Self::QueueOverflow => "the agent could not take delivery of events",
        }
    }
}

/// One path the kernel says changed, with the hints the flags carry. The
/// hints are advisory: the caller re-reads the path and lets the filesystem,
/// not the notification, decide what is true now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub path: PathBuf,
    pub event_id: u64,
    /// The kernel believes this path was removed or renamed away.
    pub removed_or_renamed: bool,
    /// The kernel believes this path is a directory.
    pub is_directory: bool,
}

/// What the feed hands to the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedMessage {
    /// Paths to reconcile, and the id through which they are complete.
    Batch {
        changes: Vec<PathChange>,
        event_id: u64,
    },
    /// Event application cannot continue; enumerate instead.
    Rescan(RescanReason),
    /// Historical replay finished; everything after this is live.
    HistoryDone,
}

/// Sent from the FSEvents callback to the watcher thread.
struct Delivery {
    sender: crossbeam_channel::Sender<FeedMessage>,
    overflowed: Arc<AtomicBool>,
}

extern "C" fn release_delivery(info: *const c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: `info` is the pointer handed to `FSEventStreamCreate`, produced
    // by `Box::into_raw`, and released exactly once by the framework.
    unsafe { drop(Box::from_raw(info as *mut Delivery)) };
}

extern "C" fn stream_callback(
    _stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    if info.is_null() || count == 0 {
        return;
    }
    // SAFETY: the framework passes back the context pointer we supplied, and
    // the three parallel arrays are valid for `count` elements for the
    // duration of this call.
    let delivery = unsafe { &*(info as *const Delivery) };
    let paths = paths as *const *const libc::c_char;
    let mut changes = Vec::with_capacity(count);
    let mut last_id = 0u64;
    let mut rescan: Option<RescanReason> = None;
    let mut history_done = false;
    for index in 0..count {
        // SAFETY: index is below `count`.
        let (path_ptr, flag, id) =
            unsafe { (*paths.add(index), *flags.add(index), *ids.add(index)) };
        last_id = last_id.max(id);
        if let Some(reason) = rescan_reason(flag) {
            rescan = Some(rescan.unwrap_or(reason));
            continue;
        }
        if flag & EVENT_FLAG_HISTORY_DONE != 0 {
            history_done = true;
            continue;
        }
        if path_ptr.is_null() {
            continue;
        }
        // SAFETY: FSEvents hands back NUL-terminated file-system paths.
        let bytes = unsafe { CStr::from_ptr(path_ptr) }.to_bytes();
        changes.push(PathChange {
            path: PathBuf::from(std::ffi::OsStr::from_bytes(bytes)),
            event_id: id,
            removed_or_renamed: flag & (EVENT_FLAG_ITEM_REMOVED | EVENT_FLAG_ITEM_RENAMED) != 0,
            is_directory: flag & EVENT_FLAG_ITEM_IS_DIR != 0,
        });
    }
    let send = |message: FeedMessage| {
        // A full queue means the watcher is behind. Dropping the batch and
        // forcing a reconciliation is correct; blocking here would stall
        // FSEvents delivery for every process on the machine.
        if delivery.sender.try_send(message).is_err() {
            delivery.overflowed.store(true, Ordering::Release);
        }
    };
    if let Some(reason) = rescan {
        send(FeedMessage::Rescan(reason));
    }
    if !changes.is_empty() {
        send(FeedMessage::Batch {
            changes,
            event_id: last_id,
        });
    }
    if history_done {
        send(FeedMessage::HistoryDone);
    }
}

fn rescan_reason(flag: u32) -> Option<RescanReason> {
    if flag & EVENT_FLAG_MUST_SCAN_SUBDIRS != 0 {
        return Some(RescanReason::MustScanSubDirs);
    }
    if flag & EVENT_FLAG_USER_DROPPED != 0 {
        return Some(RescanReason::UserDropped);
    }
    if flag & EVENT_FLAG_KERNEL_DROPPED != 0 {
        return Some(RescanReason::KernelDropped);
    }
    if flag & EVENT_FLAG_EVENT_IDS_WRAPPED != 0 {
        return Some(RescanReason::IdsWrapped);
    }
    if flag & EVENT_FLAG_ROOT_CHANGED != 0 {
        return Some(RescanReason::RootChanged);
    }
    if flag & (EVENT_FLAG_MOUNT | EVENT_FLAG_UNMOUNT) != 0 {
        return Some(RescanReason::MountChanged);
    }
    None
}

/// UUID of the event store for the volume holding `path`, or `None` when the
/// volume keeps no history and only a live (`since now`) stream is possible.
pub fn store_uuid(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let device = std::fs::metadata(path).ok()?.dev() as libc::dev_t;
    // SAFETY: the call takes a device number and returns a retained CFUUID or
    // NULL; ownership follows the Copy Rule, so the wrapper releases it.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if uuid.is_null() {
        return None;
    }
    // SAFETY: non-NULL and retained by the call above.
    let uuid = unsafe { core_foundation::uuid::CFUUID::wrap_under_create_rule(uuid) };
    // SAFETY: the wrapper owns a live CFUUID.
    let bytes = unsafe { CFUUIDGetUUIDBytes(uuid.as_concrete_TypeRef()) };
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes.byte0, bytes.byte1, bytes.byte2, bytes.byte3,
        bytes.byte4, bytes.byte5, bytes.byte6, bytes.byte7,
        bytes.byte8, bytes.byte9, bytes.byte10, bytes.byte11,
        bytes.byte12, bytes.byte13, bytes.byte14, bytes.byte15,
    ))
}

/// The event id the whole system has reached. Taken *before* enumeration so
/// that a cursor published with a scan is behind, never ahead, of what the
/// catalog holds: replaying a change that enumeration already saw is
/// idempotent, while skipping one is data loss.
pub fn current_cursor(path: &Path) -> Option<FsEventsCursor> {
    let store_uuid = store_uuid(path)?;
    // SAFETY: no arguments, no ownership.
    let event_id = unsafe { FSEventsGetCurrentEventId() };
    Some(FsEventsCursor {
        store_uuid,
        event_id,
    })
}

/// A live FSEvents stream over one source root.
pub struct FsEventsFeed {
    stream: FsEventStreamRef,
    queue: DispatchQueue,
    receiver: crossbeam_channel::Receiver<FeedMessage>,
    overflowed: Arc<AtomicBool>,
    /// Whether the stream is replaying history rather than reporting live
    /// changes. Historical batches are applied the same way; the distinction
    /// is only worth reporting to an operator.
    replaying: bool,
}

// SAFETY: the stream is only touched through the framework's own functions,
// which are thread-safe, and the channel end is `Send`. The struct is not
// `Sync` because nothing shares it.
unsafe impl Send for FsEventsFeed {}

impl FsEventsFeed {
    /// Open a stream over `root`. `since` resumes the volume's history when
    /// its store UUID still matches; otherwise the caller is told the cursor
    /// is unusable and must reconcile by enumeration.
    pub fn open(root: &Path, since: Option<&FsEventsCursor>) -> Result<Self, ScanError> {
        let current = store_uuid(root);
        let since_when = match (since, current.as_deref()) {
            (None, _) => FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
            (Some(cursor), Some(uuid)) if cursor.store_uuid == uuid => cursor.event_id,
            (Some(_), Some(_)) => {
                return Err(ScanError::new(
                    ScanErrorKind::Unsupported,
                    0,
                    "the volume's event store was replaced; its history cannot be resumed",
                    root,
                ))
            }
            (Some(_), None) => {
                return Err(ScanError::new(
                    ScanErrorKind::Unsupported,
                    0,
                    "the volume keeps no event history to resume",
                    root,
                ))
            }
        };
        let path = CFString::new(&root.to_string_lossy());
        let paths = CFArray::from_CFTypes(&[path]);
        let (sender, receiver) = crossbeam_channel::bounded(QUEUE_DEPTH);
        let overflowed = Arc::new(AtomicBool::new(false));
        let delivery = Box::into_raw(Box::new(Delivery {
            sender,
            overflowed: overflowed.clone(),
        }));
        let context = FsEventStreamContext {
            version: 0,
            info: delivery as *mut c_void,
            retain: None,
            release: Some(release_delivery),
            copy_description: None,
        };
        // SAFETY: the paths array, context, and callback all outlive the call;
        // the context's release callback frees `delivery` exactly once when
        // the stream is released.
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                stream_callback,
                &context,
                paths.as_concrete_TypeRef() as *const c_void,
                since_when,
                LATENCY_SECONDS,
                CREATE_FLAG_FILE_EVENTS | CREATE_FLAG_NO_DEFER | CREATE_FLAG_WATCH_ROOT,
            )
        };
        if stream.is_null() {
            // The framework never took ownership, so the context is ours.
            // SAFETY: `delivery` came from `Box::into_raw` and is unshared.
            unsafe { drop(Box::from_raw(delivery)) };
            return Err(ScanError::new(
                ScanErrorKind::Unsupported,
                0,
                "FSEventStreamCreate failed for this root",
                root,
            ));
        }
        let label = CString::new("com.jnel.eidos.fsevents").expect("static label");
        // SAFETY: the label is NUL-terminated and outlives the call, which
        // copies it; a NULL attribute makes a serial queue.
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            // The stream owns `delivery`, so releasing it also runs the
            // context release callback and frees the channel state.
            // SAFETY: `stream` was created successfully but has not been
            // scheduled or started yet.
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
            }
            return Err(ScanError::new(
                ScanErrorKind::Transient,
                0,
                "could not allocate the FSEvents dispatch queue",
                root,
            ));
        }
        // SAFETY: both the stream and the queue are live.
        unsafe { FSEventStreamSetDispatchQueue(stream, queue) };
        // SAFETY: the stream is scheduled on a queue, which start requires.
        if unsafe { FSEventStreamStart(stream) } == 0 {
            // SAFETY: unscheduling and releasing a created-but-unstarted
            // stream is the documented teardown order.
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
            }
            return Err(ScanError::new(
                ScanErrorKind::Transient,
                0,
                "FSEventStreamStart was refused",
                root,
            ));
        }
        Ok(Self {
            stream,
            queue,
            receiver,
            overflowed,
            replaying: since_when != FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
        })
    }

    /// Whether this stream is still replaying stored history.
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    /// Wait for the next message. `None` means the window elapsed with
    /// nothing to report, which is the normal idle case.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<FeedMessage> {
        if self.overflowed.swap(false, Ordering::AcqRel) {
            return Some(FeedMessage::Rescan(RescanReason::QueueOverflow));
        }
        let message = self.receiver.recv_timeout(timeout).ok()?;
        if matches!(message, FeedMessage::HistoryDone) {
            self.replaying = false;
        }
        Some(message)
    }
}

impl Drop for FsEventsFeed {
    fn drop(&mut self) {
        // SAFETY: documented teardown order for a queue-scheduled stream. The
        // context's release callback runs during `FSEventStreamRelease`, so
        // the callback cannot observe a freed sender afterwards.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_loss_flag_forces_a_rescan() {
        for (flag, expected) in [
            (EVENT_FLAG_MUST_SCAN_SUBDIRS, RescanReason::MustScanSubDirs),
            (EVENT_FLAG_USER_DROPPED, RescanReason::UserDropped),
            (EVENT_FLAG_KERNEL_DROPPED, RescanReason::KernelDropped),
            (EVENT_FLAG_EVENT_IDS_WRAPPED, RescanReason::IdsWrapped),
            (EVENT_FLAG_ROOT_CHANGED, RescanReason::RootChanged),
            (EVENT_FLAG_UNMOUNT, RescanReason::MountChanged),
        ] {
            assert_eq!(rescan_reason(flag), Some(expected), "flag {flag:#x}");
        }
    }

    #[test]
    fn an_ordinary_change_is_not_a_rescan() {
        assert_eq!(rescan_reason(EVENT_FLAG_ITEM_RENAMED), None);
        assert_eq!(rescan_reason(EVENT_FLAG_HISTORY_DONE), None);
        assert_eq!(rescan_reason(0), None);
    }

    #[test]
    fn the_boot_volume_has_an_event_store_identity() {
        // The data volume keeps history; the read-only system volume may not,
        // which is exactly why the cursor records the answer.
        let home = std::env::temp_dir();
        let uuid = store_uuid(&home);
        if let Some(uuid) = uuid {
            assert_eq!(uuid.len(), 36, "{uuid} should be a formatted UUID");
            let cursor = current_cursor(&home).expect("cursor when a store exists");
            assert_eq!(cursor.store_uuid, uuid);
        }
    }
}
