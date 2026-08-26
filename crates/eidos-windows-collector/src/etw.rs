//! Real-time ETW session over `Microsoft-Windows-Kernel-File` and
//! `Microsoft-Windows-Kernel-Process`. Event payloads are decoded through
//! TDH's self-describing metadata (property names, types, and order come
//! from the provider manifest at run time), and only kernel object
//! identities, sizes, a bucketed extension, and process image names
//! (immediately classified) leave the callback.

use crate::access::{AccessEvent, AccessKind};
use eidos_observe::{bucket_extension, ExtensionBucket};
use std::collections::HashMap;
use std::sync::mpsc::SyncSender;
use std::sync::Mutex;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER};
use windows_sys::Win32::System::Diagnostics::Etw::TdhGetEventInformation;
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_CONTROL_CODE_DISABLE_PROVIDER, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID, EVENT_HEADER_FLAG_32_BIT_HEADER,
    EVENT_PROPERTY_INFO, EVENT_RECORD, EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, TRACE_EVENT_INFO,
    TRACE_LEVEL_VERBOSE, WNODE_FLAG_TRACED_GUID,
};

pub const SESSION_NAME: &str = "eidos-collector-access";

// {EDD08927-9CC4-4E65-B970-C2560FB5C289}
pub const KERNEL_FILE: GUID = GUID {
    data1: 0xEDD0_8927,
    data2: 0x9CC4,
    data3: 0x4E65,
    data4: [0xB9, 0x70, 0xC2, 0x56, 0x0F, 0xB5, 0xC2, 0x89],
};
// {22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}
pub const KERNEL_PROCESS: GUID = GUID {
    data1: 0x22FB_2CD6,
    data2: 0x0E7B,
    data3: 0x422B,
    data4: [0xA0, 0xC7, 0x2F, 0xAD, 0x1F, 0xD0, 0xE7, 0x16],
};

// Kernel-File keywords.
const KERNEL_FILE_KEYWORD_FILEIO: u64 = 0x20;
const KERNEL_FILE_KEYWORD_CREATE: u64 = 0x80;
const KERNEL_FILE_KEYWORD_READ: u64 = 0x100;
const KERNEL_FILE_KEYWORD_WRITE: u64 = 0x200;
const KERNEL_FILE_KEYWORD_DELETE_PATH: u64 = 0x400;
const KERNEL_FILE_KEYWORD_RENAME_SETLINK_PATH: u64 = 0x800;
const KERNEL_FILE_KEYWORD_CREATE_NEW_FILE: u64 = 0x1000;
// Kernel-Process keyword.
const WINEVENT_KEYWORD_PROCESS: u64 = 0x10;

// Kernel-File event ids.
const FILE_CREATE: u16 = 12;
const FILE_CLOSE: u16 = 14;
const FILE_READ: u16 = 15;
const FILE_WRITE: u16 = 16;
const FILE_SET_DELETE: u16 = 18;
const FILE_RENAME: u16 = 19;
const FILE_DELETE_PATH: u16 = 26;
const FILE_RENAME_PATH: u16 = 27;
const FILE_CREATE_NEW: u16 = 30;
const FILE_EVENTS: [u16; 9] = [
    FILE_CREATE,
    FILE_CLOSE,
    FILE_READ,
    FILE_WRITE,
    FILE_SET_DELETE,
    FILE_RENAME,
    FILE_DELETE_PATH,
    FILE_RENAME_PATH,
    FILE_CREATE_NEW,
];
// Kernel-Process event ids.
const PROCESS_START: u16 = 1;
const PROCESS_STOP: u16 = 2;

/// What the consumer hands to the lane thread. Image names are owned
/// strings only until the lane classifies them.
#[derive(Debug)]
pub enum TraceEvent {
    Access(AccessEvent),
    ProcessStart { pid: u32, image: String },
    ProcessStop { pid: u32 },
}

pub struct Session {
    handle: CONTROLTRACE_HANDLE,
    properties: Vec<u8>,
    name: Vec<u16>,
}

// SAFETY: the control handle is a kernel object usable from any thread.
unsafe impl Send for Session {}

fn properties_buffer() -> Vec<u8> {
    let size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 2 * 1024 * 2;
    let mut buffer = vec![0u8; size];
    // SAFETY: buffer is zeroed and at least the struct size.
    let properties = unsafe { &mut *(buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES) };
    properties.Wnode.BufferSize = size as u32;
    properties.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    properties.Wnode.ClientContext = 1; // QPC timestamps
    properties.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    properties.BufferSize = 64;
    properties.MinimumBuffers = 16;
    properties.MaximumBuffers = 64;
    properties.FlushTimer = 1;
    properties.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    properties.LogFileNameOffset = properties.LoggerNameOffset + 2 * 1024;
    buffer
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Session {
    /// Start the session, stopping a stale instance with the same name.
    pub fn start() -> Result<Self, u32> {
        let name = wide(SESSION_NAME);
        let mut properties = properties_buffer();
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        // SAFETY: properties buffer laid out as documented; name NUL-terminated.
        let mut status = unsafe {
            StartTraceW(
                &mut handle,
                name.as_ptr(),
                properties.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
            )
        };
        if status == ERROR_ALREADY_EXISTS {
            let mut stale = properties_buffer();
            // SAFETY: as above; stopping by name needs no handle.
            unsafe {
                ControlTraceW(
                    CONTROLTRACE_HANDLE { Value: 0 },
                    name.as_ptr(),
                    stale.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            properties = properties_buffer();
            status = unsafe {
                StartTraceW(
                    &mut handle,
                    name.as_ptr(),
                    properties.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                )
            };
        }
        if status != 0 {
            return Err(status);
        }
        let session = Self {
            handle,
            properties,
            name,
        };
        session.enable(
            &KERNEL_FILE,
            KERNEL_FILE_KEYWORD_FILEIO
                | KERNEL_FILE_KEYWORD_CREATE
                | KERNEL_FILE_KEYWORD_READ
                | KERNEL_FILE_KEYWORD_WRITE
                | KERNEL_FILE_KEYWORD_DELETE_PATH
                | KERNEL_FILE_KEYWORD_RENAME_SETLINK_PATH
                | KERNEL_FILE_KEYWORD_CREATE_NEW_FILE,
            Some(&FILE_EVENTS),
        )?;
        session.enable(
            &KERNEL_PROCESS,
            WINEVENT_KEYWORD_PROCESS,
            Some(&[PROCESS_START, PROCESS_STOP]),
        )?;
        Ok(session)
    }

    fn enable(&self, provider: &GUID, keywords: u64, event_ids: Option<&[u16]>) -> Result<(), u32> {
        // EVENT_FILTER_EVENT_ID: FilterIn (u8), Reserved (u8), Count (u16), Events[Count].
        let mut filter_bytes = Vec::new();
        let mut descriptor = EVENT_FILTER_DESCRIPTOR {
            Ptr: 0,
            Size: 0,
            Type: EVENT_FILTER_TYPE_EVENT_ID,
        };
        if let Some(ids) = event_ids {
            filter_bytes.push(1u8);
            filter_bytes.push(0u8);
            filter_bytes.extend_from_slice(&(ids.len() as u16).to_le_bytes());
            for id in ids {
                filter_bytes.extend_from_slice(&id.to_le_bytes());
            }
            descriptor.Ptr = filter_bytes.as_ptr() as u64;
            descriptor.Size = filter_bytes.len() as u32;
        }
        let parameters = ENABLE_TRACE_PARAMETERS {
            Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
            EnableProperty: 0,
            ControlFlags: 0,
            SourceId: GUID {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
            EnableFilterDesc: if event_ids.is_some() {
                &mut descriptor
            } else {
                std::ptr::null_mut()
            },
            FilterDescCount: u32::from(event_ids.is_some()),
        };
        // SAFETY: parameters and filter memory outlive the call.
        let status = unsafe {
            EnableTraceEx2(
                self.handle,
                provider,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                TRACE_LEVEL_VERBOSE as u8,
                keywords,
                0,
                0,
                &parameters,
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(())
    }

    /// Events the kernel dropped because buffers were full.
    pub fn events_lost(&mut self) -> u32 {
        // SAFETY: querying refreshes the properties buffer in place.
        let status = unsafe {
            ControlTraceW(
                self.handle,
                std::ptr::null(),
                self.properties.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        if status != 0 {
            return 0;
        }
        // SAFETY: buffer holds the struct.
        let properties = unsafe { &*(self.properties.as_ptr() as *const EVENT_TRACE_PROPERTIES) };
        properties.EventsLost + properties.RealTimeBuffersLost
    }

    pub fn stop(mut self) -> u32 {
        let lost = self.events_lost();
        self.teardown();
        lost
    }

    /// Disable the providers and stop the session. Idempotent: a session that
    /// has already been torn down carries a zeroed handle and does nothing.
    fn teardown(&mut self) {
        if self.handle.Value == 0 {
            return;
        }
        // SAFETY: handle from StartTraceW; providers are disabled first so
        // a later session start does not inherit their enable state.
        unsafe {
            EnableTraceEx2(
                self.handle,
                &KERNEL_FILE,
                EVENT_CONTROL_CODE_DISABLE_PROVIDER,
                0,
                0,
                0,
                0,
                std::ptr::null(),
            );
            EnableTraceEx2(
                self.handle,
                &KERNEL_PROCESS,
                EVENT_CONTROL_CODE_DISABLE_PROVIDER,
                0,
                0,
                0,
                0,
                std::ptr::null(),
            );
            ControlTraceW(
                self.handle,
                self.name.as_ptr(),
                self.properties.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                EVENT_TRACE_CONTROL_STOP,
            );
        }
        self.handle = CONTROLTRACE_HANDLE { Value: 0 };
    }
}

impl Drop for Session {
    /// A session that started but was never handed back to the caller — a
    /// provider that would not enable, a consumer thread that would not spawn
    /// — must not be left running with nobody draining it. Without this the
    /// named session and its providers survived until the next window's
    /// stale-session sweep, costing an observation window and ETW resources.
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Stop any session left over from a crashed collector.
pub fn stop_stale_session() {
    let name = wide(SESSION_NAME);
    let mut properties = properties_buffer();
    // SAFETY: stopping by name.
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            properties.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}

// ----- consumer ---------------------------------------------------------------

struct ConsumerState {
    sender: SyncSender<TraceEvent>,
    layouts: Mutex<HashMap<(u128, u16, u8), Option<Layout>>>,
    pub events: std::sync::atomic::AtomicU64,
    /// Events discarded because the aggregator was not keeping up.
    pub dropped: std::sync::atomic::AtomicU64,
}

/// Blocks in `ProcessTrace` until the session stops; run on its own thread.
pub fn consume(
    sender: SyncSender<TraceEvent>,
    events: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<(), u32> {
    let state = Box::new(ConsumerState {
        sender,
        layouts: Mutex::new(HashMap::new()),
        events: std::sync::atomic::AtomicU64::new(0),
        dropped: std::sync::atomic::AtomicU64::new(0),
    });
    let mut name = wide(SESSION_NAME);
    // SAFETY: the logfile struct is zero-initialised and fully specified.
    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    logfile.LoggerName = name.as_mut_ptr();
    logfile.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
    logfile.Anonymous2.EventRecordCallback = Some(on_event);
    let state_ptr = Box::into_raw(state);
    logfile.Context = state_ptr as *mut _;
    // SAFETY: logfile fully initialised; state outlives ProcessTrace.
    let handle = unsafe { OpenTraceW(&mut logfile) };
    if handle.Value == u64::MAX {
        let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        // SAFETY: reclaim the box we leaked above.
        drop(unsafe { Box::from_raw(state_ptr) });
        return Err(error);
    }
    // SAFETY: handle from OpenTraceW; blocks until the session stops.
    let status = unsafe { ProcessTrace(&handle, 1, std::ptr::null(), std::ptr::null()) };
    unsafe { CloseTrace(handle) };
    // SAFETY: ProcessTrace has returned, so no callback can run.
    let state = unsafe { Box::from_raw(state_ptr) };
    events.store(
        state.events.load(std::sync::atomic::Ordering::Relaxed),
        std::sync::atomic::Ordering::Relaxed,
    );
    dropped.store(
        state.dropped.load(std::sync::atomic::Ordering::Relaxed),
        std::sync::atomic::Ordering::Relaxed,
    );
    if status != 0 {
        return Err(status);
    }
    Ok(())
}

fn guid_key(guid: &GUID) -> u128 {
    ((guid.data1 as u128) << 96)
        | ((guid.data2 as u128) << 80)
        | ((guid.data3 as u128) << 64)
        | u128::from_be_bytes([
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            guid.data4[0],
            guid.data4[1],
            guid.data4[2],
            guid.data4[3],
            guid.data4[4],
            guid.data4[5],
            guid.data4[6],
            guid.data4[7],
        ])
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    // SAFETY: ETW passes a valid record; Context is our boxed state.
    let record = unsafe { &*record };
    let state = unsafe { &*(record.UserContext as *const ConsumerState) };
    state
        .events
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let header = &record.EventHeader;
    let provider = guid_key(&header.ProviderId);
    let id = header.EventDescriptor.Id;
    let version = header.EventDescriptor.Version;
    let pointer_size = if header.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER as u16 != 0 {
        4
    } else {
        8
    };
    let layout = {
        let mut layouts = state.layouts.lock().unwrap();
        layouts
            .entry((provider, id, version))
            .or_insert_with(|| Layout::from_record(record))
            .clone()
    };
    let Some(layout) = layout else { return };
    let user_data = unsafe {
        std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize)
    };
    let fields = layout.decode(user_data, pointer_size);
    let pid = header.ProcessId;
    let event = if provider == guid_key(&KERNEL_FILE) {
        let object = fields
            .u64("FileObject")
            .or_else(|| fields.u64("FileKey"))
            .unwrap_or(0);
        let kind = match id {
            FILE_CREATE | FILE_CREATE_NEW => AccessKind::Open,
            FILE_READ => AccessKind::Read,
            FILE_WRITE => AccessKind::Write,
            FILE_CLOSE => AccessKind::Close,
            FILE_SET_DELETE | FILE_DELETE_PATH => AccessKind::Delete,
            FILE_RENAME | FILE_RENAME_PATH => AccessKind::Rename,
            _ => return,
        };
        let extension = if kind == AccessKind::Open {
            Some(
                fields
                    .string("FileName")
                    .map(|name| extension_bucket_of_path(&name))
                    .unwrap_or(ExtensionBucket::None),
            )
        } else {
            None
        };
        TraceEvent::Access(AccessEvent {
            pid,
            kind,
            object,
            bytes: fields.u64("IOSize").unwrap_or(0),
            extension,
        })
    } else if provider == guid_key(&KERNEL_PROCESS) {
        let target = fields.u64("ProcessID").map(|p| p as u32).unwrap_or(pid);
        match id {
            PROCESS_START => TraceEvent::ProcessStart {
                pid: target,
                image: fields.string("ImageName").unwrap_or_default(),
            },
            PROCESS_STOP => TraceEvent::ProcessStop { pid: target },
            _ => return,
        }
    } else {
        return;
    };
    // Never block the ETW callback: a full queue drops the event and counts
    // it, which keeps the loss bounded and visible instead of letting the
    // backlog grow without limit.
    if state.sender.try_send(event).is_err() {
        state
            .dropped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Extension bucket of an NT path (`\Device\HarddiskVolumeN\...`); the path
/// itself is dropped by the caller.
pub fn extension_bucket_of_path(path: &str) -> ExtensionBucket {
    let name = path.rsplit('\\').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => bucket_extension(Some(extension)),
        _ => ExtensionBucket::None,
    }
}

// ----- TDH layouts ------------------------------------------------------------

#[derive(Clone, Debug)]
struct Property {
    name: String,
    in_type: u16,
    length: u16,
    length_from_property: Option<u16>,
    count: u16,
}

#[derive(Clone, Debug)]
struct Layout {
    properties: Vec<Property>,
}

#[derive(Debug)]
enum Field {
    Unsigned(u64),
    Text(String),
    Skipped,
}

struct Fields<'a> {
    layout: &'a Layout,
    values: Vec<Field>,
}

impl Fields<'_> {
    fn get(&self, name: &str) -> Option<&Field> {
        self.layout
            .properties
            .iter()
            .position(|p| p.name == name)
            .and_then(|index| self.values.get(index))
    }

    fn u64(&self, name: &str) -> Option<u64> {
        match self.get(name)? {
            Field::Unsigned(value) => Some(*value),
            _ => None,
        }
    }

    fn string(&self, name: &str) -> Option<String> {
        match self.get(name)? {
            Field::Text(value) => Some(value.clone()),
            _ => None,
        }
    }
}

const TDH_INTYPE_UNICODESTRING: u16 = 1;
const TDH_INTYPE_ANSISTRING: u16 = 2;
const TDH_INTYPE_INT8: u16 = 3;
const TDH_INTYPE_UINT8: u16 = 4;
const TDH_INTYPE_INT16: u16 = 5;
const TDH_INTYPE_UINT16: u16 = 6;
const TDH_INTYPE_INT32: u16 = 7;
const TDH_INTYPE_UINT32: u16 = 8;
const TDH_INTYPE_INT64: u16 = 9;
const TDH_INTYPE_UINT64: u16 = 10;
const TDH_INTYPE_FLOAT: u16 = 11;
const TDH_INTYPE_DOUBLE: u16 = 12;
const TDH_INTYPE_BOOLEAN: u16 = 13;
const TDH_INTYPE_BINARY: u16 = 14;
const TDH_INTYPE_GUID: u16 = 15;
const TDH_INTYPE_POINTER: u16 = 16;
const TDH_INTYPE_FILETIME: u16 = 17;
const TDH_INTYPE_SYSTEMTIME: u16 = 18;
const TDH_INTYPE_SID: u16 = 19;
const TDH_INTYPE_HEXINT32: u16 = 20;
const TDH_INTYPE_HEXINT64: u16 = 21;
const PROPERTY_STRUCT: i32 = 1;
const PROPERTY_PARAM_LENGTH: i32 = 2;
const PROPERTY_PARAM_COUNT: i32 = 4;

impl Layout {
    fn from_record(record: &EVENT_RECORD) -> Option<Layout> {
        let mut size = 0u32;
        // SAFETY: size query with a null buffer is the documented protocol.
        let status = unsafe {
            TdhGetEventInformation(record, 0, std::ptr::null(), std::ptr::null_mut(), &mut size)
        };
        if status != ERROR_INSUFFICIENT_BUFFER || size == 0 {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: buffer sized by the previous call.
        let status = unsafe {
            TdhGetEventInformation(
                record,
                0,
                std::ptr::null(),
                buffer.as_mut_ptr() as *mut TRACE_EVENT_INFO,
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        // SAFETY: TDH filled a TRACE_EVENT_INFO followed by its property array
        // and string table within `buffer`.
        let info = unsafe { &*(buffer.as_ptr() as *const TRACE_EVENT_INFO) };
        let array_offset = std::mem::offset_of!(TRACE_EVENT_INFO, EventPropertyInfoArray);
        let mut properties = Vec::with_capacity(info.TopLevelPropertyCount as usize);
        for index in 0..info.TopLevelPropertyCount as usize {
            let offset = array_offset + index * std::mem::size_of::<EVENT_PROPERTY_INFO>();
            if offset + std::mem::size_of::<EVENT_PROPERTY_INFO>() > buffer.len() {
                return None;
            }
            // SAFETY: bounds checked above; the array is repr(C).
            let property = unsafe { &*(buffer.as_ptr().add(offset) as *const EVENT_PROPERTY_INFO) };
            if property.Flags & PROPERTY_STRUCT != 0 {
                return None;
            }
            let name = wide_string_at(&buffer, property.NameOffset as usize);
            // SAFETY: non-struct properties use the nonStructType view.
            let in_type = unsafe { property.Anonymous1.nonStructType.InType };
            let (length, length_from_property) = if property.Flags & PROPERTY_PARAM_LENGTH != 0 {
                // SAFETY: flag selects the index view.
                (0, Some(unsafe { property.Anonymous3.lengthPropertyIndex }))
            } else {
                // SAFETY: flag selects the length view.
                (unsafe { property.Anonymous3.length }, None)
            };
            let count = if property.Flags & PROPERTY_PARAM_COUNT != 0 {
                0
            } else {
                // SAFETY: fixed count view.
                unsafe { property.Anonymous2.count }.max(1)
            };
            properties.push(Property {
                name,
                in_type,
                length,
                length_from_property,
                count,
            });
        }
        Some(Layout { properties })
    }

    fn decode<'a>(&'a self, data: &[u8], pointer_size: usize) -> Fields<'a> {
        let mut values = Vec::with_capacity(self.properties.len());
        let mut offset = 0usize;
        for property in &self.properties {
            if property.count == 0 {
                // Variable-count arrays are not needed by any event we consume.
                values.push(Field::Skipped);
                break;
            }
            let mut field = Field::Skipped;
            let mut truncated = false;
            for element in 0..property.count as usize {
                let remaining = &data[offset.min(data.len())..];
                let length = match property.length_from_property {
                    Some(index) => match values.get(index as usize) {
                        Some(Field::Unsigned(value)) => *value as usize,
                        _ => 0,
                    },
                    None => property.length as usize,
                };
                let (value, consumed) =
                    decode_value(property.in_type, length, remaining, pointer_size);
                if element == 0 {
                    field = value;
                }
                offset += consumed;
                if consumed == 0 {
                    truncated = true;
                    break;
                }
            }
            if truncated {
                // Nothing after a truncated property can be located.
                break;
            }
            values.push(field);
        }
        Fields {
            layout: self,
            values,
        }
    }
}

fn decode_value(in_type: u16, length: usize, data: &[u8], pointer_size: usize) -> (Field, usize) {
    let fixed = |size: usize| -> (Field, usize) {
        if data.len() < size {
            return (Field::Skipped, 0);
        }
        let mut raw = [0u8; 8];
        raw[..size.min(8)].copy_from_slice(&data[..size.min(8)]);
        (Field::Unsigned(u64::from_le_bytes(raw)), size)
    };
    match in_type {
        TDH_INTYPE_UNICODESTRING => {
            if length > 0 {
                let bytes = (length * 2).min(data.len());
                let units: Vec<u16> = data[..bytes]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                (Field::Text(String::from_utf16_lossy(&units)), bytes)
            } else {
                let mut units = Vec::new();
                let mut consumed = 0;
                for chunk in data.chunks_exact(2) {
                    consumed += 2;
                    let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
                    if unit == 0 {
                        break;
                    }
                    units.push(unit);
                }
                (Field::Text(String::from_utf16_lossy(&units)), consumed)
            }
        }
        TDH_INTYPE_ANSISTRING => {
            if length > 0 {
                let bytes = length.min(data.len());
                (
                    Field::Text(String::from_utf8_lossy(&data[..bytes]).into_owned()),
                    bytes,
                )
            } else {
                let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
                (
                    Field::Text(String::from_utf8_lossy(&data[..end]).into_owned()),
                    (end + 1).min(data.len()),
                )
            }
        }
        TDH_INTYPE_INT8 | TDH_INTYPE_UINT8 => fixed(1),
        TDH_INTYPE_INT16 | TDH_INTYPE_UINT16 => fixed(2),
        TDH_INTYPE_INT32 | TDH_INTYPE_UINT32 | TDH_INTYPE_HEXINT32 | TDH_INTYPE_BOOLEAN
        | TDH_INTYPE_FLOAT => fixed(4),
        TDH_INTYPE_INT64 | TDH_INTYPE_UINT64 | TDH_INTYPE_HEXINT64 | TDH_INTYPE_FILETIME
        | TDH_INTYPE_DOUBLE => fixed(8),
        TDH_INTYPE_POINTER => fixed(pointer_size),
        TDH_INTYPE_GUID | TDH_INTYPE_SYSTEMTIME => {
            if data.len() < 16 {
                (Field::Skipped, 0)
            } else {
                (Field::Skipped, 16)
            }
        }
        TDH_INTYPE_SID => {
            if data.len() < 8 {
                return (Field::Skipped, 0);
            }
            let size = 8 + 4 * data[1] as usize;
            (Field::Skipped, size.min(data.len()))
        }
        TDH_INTYPE_BINARY => (Field::Skipped, length.min(data.len())),
        _ => (Field::Skipped, length.min(data.len())),
    }
}

fn wide_string_at(buffer: &[u8], offset: usize) -> String {
    if offset == 0 || offset >= buffer.len() {
        return String::new();
    }
    let units: Vec<u16> = buffer[offset..]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(properties: &[(&str, u16, u16)]) -> Layout {
        Layout {
            properties: properties
                .iter()
                .map(|(name, in_type, length)| Property {
                    name: name.to_string(),
                    in_type: *in_type,
                    length: *length,
                    length_from_property: None,
                    count: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn decodes_a_kernel_file_create_payload_by_layout() {
        // Irp, FileObject, IssuingThreadId, CreateOptions, CreateAttributes,
        // ShareAccess, FileName — the Create template.
        let layout = layout(&[
            ("Irp", TDH_INTYPE_POINTER, 0),
            ("FileObject", TDH_INTYPE_POINTER, 0),
            ("IssuingThreadId", TDH_INTYPE_UINT32, 0),
            ("CreateOptions", TDH_INTYPE_UINT32, 0),
            ("CreateAttributes", TDH_INTYPE_UINT32, 0),
            ("ShareAccess", TDH_INTYPE_UINT32, 0),
            ("FileName", TDH_INTYPE_UNICODESTRING, 0),
        ]);
        let mut data = Vec::new();
        data.extend_from_slice(&0xAAAAu64.to_le_bytes());
        data.extend_from_slice(&0xBEEFu64.to_le_bytes());
        data.extend_from_slice(&7u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&3u32.to_le_bytes());
        for unit in r"\Device\HarddiskVolume3\dir\thing.RS".encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data.extend_from_slice(&[0, 0]);
        let fields = layout.decode(&data, 8);
        assert_eq!(fields.u64("FileObject"), Some(0xBEEF));
        assert_eq!(fields.u64("ShareAccess"), Some(3));
        assert_eq!(
            extension_bucket_of_path(&fields.string("FileName").unwrap()),
            ExtensionBucket::Source
        );
        assert_eq!(
            extension_bucket_of_path(r"\Device\HarddiskVolume3\.gitignore"),
            ExtensionBucket::None
        );
        // 32-bit headers shrink pointers: the first pointer is the low
        // half of the 64-bit value written above and the second its high half.
        let fields = layout.decode(&data, 4);
        assert_eq!(fields.u64("Irp"), Some(0xAAAA));
        assert_eq!(fields.u64("FileObject"), Some(0));
    }

    #[test]
    fn truncated_payloads_do_not_panic() {
        let layout = layout(&[
            ("ByteOffset", TDH_INTYPE_UINT64, 0),
            ("IOSize", TDH_INTYPE_UINT32, 0),
            ("Name", TDH_INTYPE_UNICODESTRING, 0),
        ]);
        let fields = layout.decode(&[1, 2, 3], 8);
        assert_eq!(fields.u64("ByteOffset"), None);
        assert_eq!(fields.u64("IOSize"), None);
        assert_eq!(fields.string("Name"), None);
    }
}
