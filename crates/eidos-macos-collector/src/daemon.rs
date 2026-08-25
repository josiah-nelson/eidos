use crate::endpoint_security::{Counters, Lane};
use crate::protocol::{CollectorStatus, EndpointEventCounts, Request, Response};
use eidos_observe::{
    bucket_age, bucket_depth, bucket_extension, bucket_size, AgeBucket, ApfsKind, ApfsObservation,
    BundleManifest, Capabilities, CaptureGap, ChangeOperation, CountBucket, DropCounters,
    EndpointSecurityCapability, EndpointSecurityState, FeedCursor, FeedKind, GapCause,
    HealthRecord, LifecycleEvent, LogicalChange, MachineKind, MarkRecord, ObservationBundle,
    ObservationRecord, ProcessClass, SizeBucket, Spool, SpoolLimits, StudyKey, TimeAnchor, Units,
    WorkloadSummary, SCHEMA_VERSION,
};
use fsevent_stream::ffi::{
    kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamCreateFlagWatchRoot, kFSEventStreamEventIdSinceNow,
};
use fsevent_stream::flags::StreamFlags;
use fsevent_stream::stream::create_event_stream;
use futures_util::StreamExt;
use std::ffi::{c_char, CString};
use std::fs;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_TRACKED_IDENTITIES: usize = 65_536;
const IPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Config {
    pub endpoint_security: bool,
    pub entitlement_claimed: bool,
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub export_dir: PathBuf,
}

struct Shared {
    key: Mutex<Option<StudyKey>>,
    spool: Mutex<Spool>,
    capabilities: Mutex<Capabilities>,
    drops: Mutex<DropCounters>,
    gaps: Mutex<Vec<CaptureGap>>,
    cursor: Mutex<Option<FeedCursor>>,
    endpoint_counts: Arc<Counters>,
    started: Instant,
    export_dir: PathBuf,
    build_hash: String,
    config_hash: String,
}

#[repr(C)]
#[derive(Default)]
struct NativeFileMetadata {
    logical_size: u64,
    allocated_size: u64,
    ubiquitous: libc::c_int,
    placeholder: libc::c_int,
}

extern "C" {
    fn eidos_query_file_metadata(
        file_name: *const c_char,
        output: *mut NativeFileMetadata,
    ) -> libc::c_int;
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    prepare_directory(&config.data_dir)?;
    prepare_directory(&config.export_dir)?;
    let clean_marker = config.data_dir.join("clean-shutdown");
    let clean_prior_shutdown = clean_marker.exists();
    if clean_prior_shutdown {
        fs::remove_file(&clean_marker)?;
    }

    let spool = Spool::open(&config.data_dir.join("spool.db"), SpoolLimits::default())?;
    let (endpoint_lane, endpoint_state) = Lane::start(config.endpoint_security);
    let running_as_root = unsafe { libc::geteuid() } == 0;
    let tcc_full_disk_access = match endpoint_state {
        EndpointSecurityState::Available => Some(true),
        EndpointSecurityState::NotPermitted => Some(false),
        _ => None,
    };
    let capabilities = Capabilities {
        fsevents: true,
        endpoint_security: EndpointSecurityCapability {
            state: endpoint_state,
            entitlement_claimed: config.entitlement_claimed,
            tcc_full_disk_access,
            running_as_root,
        },
        apfs: true,
    };
    let build_hash = blake3::hash(
        option_env!("EIDOS_BUILD_REVISION")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .as_bytes(),
    )
    .to_hex()
    .to_string();
    let config_hash = blake3::hash(
        format!(
            "endpoint_security={};entitlement_claimed={}",
            config.endpoint_security, config.entitlement_claimed
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string();
    let shared = Arc::new(Shared {
        key: Mutex::new(None),
        spool: Mutex::new(spool),
        capabilities: Mutex::new(capabilities),
        drops: Mutex::new(DropCounters::default()),
        gaps: Mutex::new(Vec::new()),
        cursor: Mutex::new(load_cursor(&config.data_dir.join("fsevents.cursor"))),
        endpoint_counts: endpoint_lane.counters.clone(),
        started: Instant::now(),
        export_dir: config.export_dir.clone(),
        build_hash,
        config_hash,
    });
    append_health(&shared, LifecycleEvent::Started, Some(clean_prior_shutdown))?;
    tracing::info!(
        endpoint_security = ?endpoint_state,
        running_as_root,
        "collector started"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let ipc_thread = start_ipc(config.socket.clone(), shared.clone(), shutdown.clone())?;
    let (feed_shutdown_tx, feed_shutdown_rx) = tokio::sync::watch::channel(false);
    let feed_task = tokio::spawn(run_fsevents(
        config.data_dir.join("fsevents.cursor"),
        shared.clone(),
        feed_shutdown_rx,
    ));
    let health_task = tokio::spawn(run_health(shared.clone(), feed_shutdown_tx.subscribe()));

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    shutdown.store(true, Ordering::Release);
    let _ = feed_shutdown_tx.send(true);
    feed_task.await??;
    health_task.await??;
    ipc_thread
        .join()
        .map_err(|_| anyhow::anyhow!("IPC thread panicked"))??;
    append_health(&shared, LifecycleEvent::Heartbeat, Some(true))?;
    fs::write(clean_marker, b"clean\n")?;
    drop(endpoint_lane);
    tracing::info!("collector stopped");
    Ok(())
}

fn prepare_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o750))?;
    Ok(())
}

fn start_ipc(
    socket: PathBuf,
    shared: Arc<Shared>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<std::thread::JoinHandle<anyhow::Result<()>>> {
    if let Ok(metadata) = fs::symlink_metadata(&socket) {
        if !metadata.file_type().is_socket() {
            anyhow::bail!("refusing to replace a non-socket IPC entry");
        }
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    set_admin_group(&socket);
    listener.set_nonblocking(true)?;
    Ok(std::thread::Builder::new()
        .name("collector-ipc".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => handle_connection(stream, &shared),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        })?)
}

fn set_admin_group(path: &Path) {
    let Ok(name) = CString::new("admin") else {
        return;
    };
    let group = unsafe { libc::getgrnam(name.as_ptr()) };
    if group.is_null() {
        return;
    }
    let Ok(file_name) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    unsafe {
        libc::chown(file_name.as_ptr(), u32::MAX, (*group).gr_gid);
    }
}

fn handle_connection(mut stream: UnixStream, shared: &Shared) {
    let response = (|| -> anyhow::Result<Response> {
        stream.set_write_timeout(Some(IPC_TIMEOUT))?;
        let line = read_request(&mut stream)?;
        match serde_json::from_str::<Request>(&line)? {
            Request::Status => Ok(Response::Status {
                status: status(shared)?,
            }),
            Request::SessionKey { bytes } => {
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("study key must be 32 bytes"))?;
                *shared.key.lock().expect("study key lock") = Some(StudyKey::from_bytes(bytes));
                Ok(Response::Accepted)
            }
            Request::Mark { label } => {
                if label.is_empty() || label.len() > 256 {
                    anyhow::bail!("mark label must contain 1 to 256 bytes");
                }
                let key = shared.key.lock().expect("study key lock");
                let key = key
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no user-session study key is loaded"))?;
                let record = ObservationRecord::Mark(MarkRecord {
                    at: time_anchor(),
                    marker: key.token("mark", label.as_bytes()),
                });
                shared.spool.lock().expect("spool lock").append(&record)?;
                Ok(Response::Accepted)
            }
            Request::Export => {
                let file = export(shared)?;
                Ok(Response::Exported {
                    staged_file: file.to_string_lossy().into_owned(),
                })
            }
        }
    })()
    .unwrap_or_else(|error| Response::Error {
        message: error.to_string(),
    });
    let _ = serde_json::to_writer(&mut stream, &response);
    let _ = stream.write_all(b"\n");
}

fn read_request(stream: &mut UnixStream) -> std::io::Result<String> {
    let deadline = Instant::now() + IPC_TIMEOUT;
    let mut request = Vec::with_capacity(1024);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(std::io::ErrorKind::TimedOut)?;
        stream.set_read_timeout(Some(remaining))?;
        let mut buffer = [0u8; 4096];
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let newline = buffer[..count].iter().position(|byte| *byte == b'\n');
        request.extend_from_slice(&buffer[..newline.map_or(count, |index| index + 1)]);
        if request.len() > 65_536 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if newline.is_some() {
            return String::from_utf8(request)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
    }
}

fn status(shared: &Shared) -> anyhow::Result<CollectorStatus> {
    Ok(CollectorStatus {
        schema: SCHEMA_VERSION.into(),
        running: true,
        uptime_s: shared.started.elapsed().as_secs(),
        key_loaded: shared.key.lock().expect("study key lock").is_some(),
        capabilities: shared
            .capabilities
            .lock()
            .expect("capabilities lock")
            .clone(),
        feed_cursor: shared.cursor.lock().expect("cursor lock").clone(),
        spool: shared.spool.lock().expect("spool lock").stats()?.into(),
        endpoint_events: shared.endpoint_counts.snapshot(),
    })
}

fn export(shared: &Shared) -> anyhow::Result<PathBuf> {
    let now = utc_ns();
    let bundle = ObservationBundle {
        manifest: BundleManifest {
            schema: SCHEMA_VERSION.into(),
            build_hash: shared.build_hash.clone(),
            config_hash: shared.config_hash.clone(),
            created: time_anchor(),
            capabilities: shared
                .capabilities
                .lock()
                .expect("capabilities lock")
                .clone(),
            capture_gaps: shared.gaps.lock().expect("gaps lock").clone(),
            drops: shared.drops.lock().expect("drops lock").clone(),
            units: Units::default(),
        },
        records: shared.spool.lock().expect("spool lock").records()?,
    };
    let file = shared
        .export_dir
        .join(format!("observation-{now}.eidos-observation.zst"));
    eidos_observe::write_bundle(&file, &bundle)?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640))?;
    set_admin_group(&file);
    Ok(file)
}

async fn run_fsevents(
    cursor_file: PathBuf,
    shared: Arc<Shared>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let since = shared
        .cursor
        .lock()
        .expect("cursor lock")
        .as_ref()
        .and_then(|cursor| u64::from_str_radix(&cursor.opaque, 16).ok())
        .unwrap_or(kFSEventStreamEventIdSinceNow);
    let flags = kFSEventStreamCreateFlagFileEvents
        | kFSEventStreamCreateFlagNoDefer
        | kFSEventStreamCreateFlagWatchRoot;
    let (mut stream, mut handler) =
        create_event_stream([Path::new("/")], since, Duration::from_secs(1), flags)?;
    let mut state = LogicalState::new(MAX_TRACKED_IDENTITIES);
    let mut pending_rename = None;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            batch = stream.next() => {
                let Some(batch) = batch else { break };
                let mut last_id = None;
                for event in batch {
                    last_id = Some(event.id);
                    process_event(
                        &shared,
                        event,
                        &mut state,
                        &mut pending_rename,
                    )?;
                }
                if let Some(id) = last_id {
                    let cursor = FeedCursor {
                        feed: FeedKind::Fsevents,
                        version: 1,
                        opaque: format!("{id:016x}"),
                    };
                    save_cursor(&cursor_file, id)?;
                    *shared.cursor.lock().expect("cursor lock") = Some(cursor);
                }
            }
        }
    }
    handler.abort();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_event(
    shared: &Shared,
    event: fsevent_stream::stream::Event,
    state: &mut LogicalState,
    pending_rename: &mut Option<(Instant, eidos_observe::ObjectToken)>,
) -> anyhow::Result<()> {
    update_health_flags(shared, event.flags)?;
    let key = shared.key.lock().expect("study key lock");
    let Some(key) = key.as_ref() else {
        record_keyless_gap(shared);
        return Ok(());
    };
    let object = key.token("object", event.path.as_os_str().as_bytes());
    let parent = event.path.parent().unwrap_or(Path::new("/"));
    let subtree = key.token("subtree", parent.as_os_str().as_bytes());
    let operation = if event.flags.contains(StreamFlags::ITEM_REMOVED) {
        ChangeOperation::Delete
    } else if event.flags.contains(StreamFlags::ITEM_CREATED) {
        ChangeOperation::Create
    } else if event.flags.contains(StreamFlags::ITEM_RENAMED) {
        ChangeOperation::Rename
    } else {
        ChangeOperation::Update
    };
    let edit_count = increment(&mut state.edits, object.clone());
    let children = increment(&mut state.fan_out, subtree.clone());
    let delete_recreate_age = match operation {
        ChangeOperation::Delete => {
            state.deleted.put(object.clone(), Instant::now());
            None
        }
        ChangeOperation::Create => state
            .deleted
            .pop(&object)
            .map(|when| bucket_age(when.elapsed().as_secs())),
        _ => None,
    };
    let rename_pair = if operation == ChangeOperation::Rename {
        match pending_rename.take() {
            Some((when, token)) if when.elapsed() <= Duration::from_secs(2) => Some(token),
            _ => {
                let token = key.token("rename", &event.id.to_le_bytes());
                *pending_rename = Some((Instant::now(), token.clone()));
                Some(token)
            }
        }
    } else {
        None
    };
    let extension = event.path.extension().and_then(|value| value.to_str());
    let record = ObservationRecord::LogicalChange(LogicalChange {
        at: time_anchor(),
        object: object.clone(),
        subtree: subtree.clone(),
        operation,
        rename_pair,
        size: SizeBucket::Unknown,
        extension: bucket_extension(extension),
        depth: bucket_depth(event.path.components().count().saturating_sub(1)),
        edit_count: CountBucket::from(edit_count),
        delete_recreate_age,
        fan_out: CountBucket::from(children),
        backlog_age: AgeBucket::Immediate,
    });
    shared.spool.lock().expect("spool lock").append(&record)?;
    observe_apfs(shared, &event, object, subtree)?;
    Ok(())
}

fn record_keyless_gap(shared: &Shared) {
    let now = time_anchor();
    let mut gaps = shared.gaps.lock().expect("gaps lock");
    if let Some(gap) = gaps
        .last_mut()
        .filter(|gap| gap.cause == GapCause::KeyUnavailable)
    {
        gap.ended_monotonic_ns = now.monotonic_ns;
        gap.estimated_events = Some(gap.estimated_events.unwrap_or_default().saturating_add(1));
    } else {
        gaps.push(CaptureGap {
            started_monotonic_ns: now.monotonic_ns,
            ended_monotonic_ns: now.monotonic_ns,
            cause: GapCause::KeyUnavailable,
            estimated_events: Some(1),
        });
    }
}

struct LogicalState {
    edits: lru::LruCache<eidos_observe::ObjectToken, u64>,
    deleted: lru::LruCache<eidos_observe::ObjectToken, Instant>,
    fan_out: lru::LruCache<eidos_observe::ObjectToken, u64>,
}

impl LogicalState {
    fn new(limit: usize) -> Self {
        let capacity = NonZeroUsize::new(limit).expect("logical state capacity is nonzero");
        Self {
            edits: lru::LruCache::new(capacity),
            deleted: lru::LruCache::new(capacity),
            fan_out: lru::LruCache::new(capacity),
        }
    }
}

fn increment(
    values: &mut lru::LruCache<eidos_observe::ObjectToken, u64>,
    key: eidos_observe::ObjectToken,
) -> u64 {
    if let Some(value) = values.get_mut(&key) {
        *value = value.saturating_add(1);
        *value
    } else {
        values.put(key, 1);
        1
    }
}

fn update_health_flags(shared: &Shared, flags: StreamFlags) -> anyhow::Result<()> {
    let mut drops = shared.drops.lock().expect("drops lock");
    let now = time_anchor();
    let mut gap = None;
    if flags.contains(StreamFlags::MUST_SCAN_SUBDIRS) {
        drops.coalesced += 1;
    }
    if flags.contains(StreamFlags::KERNEL_DROPPED) {
        drops.kernel += 1;
        drops.overflows += 1;
        gap = Some(GapCause::KernelDrop);
    }
    if flags.contains(StreamFlags::USER_DROPPED) {
        drops.user += 1;
        drops.overflows += 1;
        gap = Some(GapCause::UserDrop);
    }
    if flags.contains(StreamFlags::ROOT_CHANGED) {
        drops.root_changes += 1;
        gap = Some(GapCause::RootChanged);
    }
    drop(drops);
    if let Some(cause) = gap {
        shared.gaps.lock().expect("gaps lock").push(CaptureGap {
            started_monotonic_ns: now.monotonic_ns,
            ended_monotonic_ns: now.monotonic_ns,
            cause,
            estimated_events: None,
        });
    }
    if flags.contains(StreamFlags::MOUNT) {
        append_health(shared, LifecycleEvent::Mounted, None)?;
    }
    if flags.contains(StreamFlags::UNMOUNT) {
        append_health(shared, LifecycleEvent::Unmounted, None)?;
    }
    Ok(())
}

fn observe_apfs(
    shared: &Shared,
    event: &fsevent_stream::stream::Event,
    object: eidos_observe::ObjectToken,
    volume: eidos_observe::ObjectToken,
) -> anyhow::Result<()> {
    let mut kinds = Vec::new();
    if event.flags.contains(StreamFlags::ITEM_CLONED) {
        kinds.push(ApfsKind::Clone);
    }
    if event.flags.contains(StreamFlags::ITEM_XATTR_MOD) {
        kinds.push(ApfsKind::ExtendedAttribute);
    }
    if event.flags.contains(StreamFlags::IS_HARDLINK) {
        kinds.push(ApfsKind::NativeIdentity);
    }
    if event.path.extension().is_some_and(|value| value == "app") {
        kinds.push(ApfsKind::Package);
    }
    let metadata = native_metadata(&event.path);
    if let Some(metadata) = &metadata {
        if metadata.logical_size > metadata.allocated_size.saturating_mul(2)
            && metadata.logical_size > 0
        {
            kinds.push(ApfsKind::Sparse);
        }
        if metadata.placeholder != 0 {
            kinds.push(ApfsKind::CloudPlaceholder);
        }
    }
    for kind in kinds {
        let record = ObservationRecord::Apfs(ApfsObservation {
            at: time_anchor(),
            volume: volume.clone(),
            object: object.clone(),
            kind,
            prevalence: CountBucket::One,
            size: metadata
                .as_ref()
                .map(|value| bucket_size(value.logical_size))
                .unwrap_or(SizeBucket::Unknown),
        });
        shared.spool.lock().expect("spool lock").append(&record)?;
    }
    Ok(())
}

fn native_metadata(path: &Path) -> Option<NativeFileMetadata> {
    let file_name = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut output = NativeFileMetadata::default();
    (unsafe { eidos_query_file_metadata(file_name.as_ptr(), &mut output) } != 0).then_some(output)
}

async fn run_health(
    shared: Arc<Shared>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut prior_wall = SystemTime::now();
    let mut prior_monotonic = Instant::now();
    let mut prior_endpoint = EndpointEventCounts::default();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = interval.tick() => {
                let wall = SystemTime::now();
                let monotonic = Instant::now();
                let wall_elapsed = wall.duration_since(prior_wall).unwrap_or_default();
                let monotonic_elapsed = monotonic.duration_since(prior_monotonic);
                if wall_elapsed > monotonic_elapsed + Duration::from_secs(5) {
                    append_health(&shared, LifecycleEvent::Sleep, None)?;
                    append_health(&shared, LifecycleEvent::Wake, None)?;
                } else if monotonic_elapsed > wall_elapsed + Duration::from_secs(5) {
                    append_health(&shared, LifecycleEvent::ClockJump, None)?;
                }
                append_health(&shared, LifecycleEvent::Heartbeat, None)?;
                let endpoint = shared.endpoint_counts.snapshot();
                if endpoint.opens > prior_endpoint.opens
                    || endpoint.closes > prior_endpoint.closes
                    || endpoint.mappings > prior_endpoint.mappings
                    || endpoint.executions > prior_endpoint.executions
                {
                    let record = ObservationRecord::Workload(WorkloadSummary {
                        at: time_anchor(),
                        process: ProcessClass::Other,
                        opens: CountBucket::from(endpoint.opens - prior_endpoint.opens),
                        closes: CountBucket::from(endpoint.closes - prior_endpoint.closes),
                        mappings: CountBucket::from(endpoint.mappings - prior_endpoint.mappings),
                        executions: CountBucket::from(endpoint.executions - prior_endpoint.executions),
                        changed_objects: CountBucket::Zero,
                    });
                    shared.spool.lock().expect("spool lock").append(&record)?;
                }
                prior_endpoint = endpoint;
                prior_wall = wall;
                prior_monotonic = monotonic;
            }
        }
    }
    Ok(())
}

fn append_health(
    shared: &Shared,
    lifecycle: LifecycleEvent,
    clean_prior_shutdown: Option<bool>,
) -> anyhow::Result<()> {
    let (cpu_millis, resident) = resource_use();
    let record = ObservationRecord::Health(HealthRecord {
        at: time_anchor(),
        os_build: os_build(),
        machine: machine_kind(),
        lifecycle,
        clean_prior_shutdown,
        feed_cursor: shared.cursor.lock().expect("cursor lock").clone(),
        drops: shared.drops.lock().expect("drops lock").clone(),
        cpu_millis,
        resident_bytes_bucket: bucket_size(resident),
    });
    shared.spool.lock().expect("spool lock").append(&record)?;
    Ok(())
}

fn resource_use() -> (u64, u64) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return (0, 0);
    }
    let usage = unsafe { usage.assume_init() };
    let millis = (usage.ru_utime.tv_sec as u64 + usage.ru_stime.tv_sec as u64) * 1_000
        + (usage.ru_utime.tv_usec as u64 + usage.ru_stime.tv_usec as u64) / 1_000;
    (millis, usage.ru_maxrss as u64)
}

fn os_build() -> String {
    command_value("/usr/bin/sw_vers", &["-buildVersion"]).unwrap_or_else(|| "unknown".into())
}

fn machine_kind() -> MachineKind {
    match command_value("/usr/sbin/sysctl", &["-n", "kern.hv_vmm_present"]).as_deref() {
        Some("1") => MachineKind::Virtual,
        Some("0") => MachineKind::Physical,
        _ => MachineKind::Unknown,
    }
}

fn command_value(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn time_anchor() -> TimeAnchor {
    TimeAnchor {
        monotonic_ns: monotonic_ns(),
        utc_ns: utc_ns(),
    }
}

fn monotonic_ns() -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut value) } == 0 {
        value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
    } else {
        0
    }
}

fn utc_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn load_cursor(file: &Path) -> Option<FeedCursor> {
    let value = fs::read_to_string(file).ok()?.trim().parse::<u64>().ok()?;
    Some(FeedCursor {
        feed: FeedKind::Fsevents,
        version: 1,
        opaque: format!("{value:016x}"),
    })
}

fn save_cursor(file: &Path, value: u64) -> anyhow::Result<()> {
    let temporary = file.with_extension("cursor.tmp");
    fs::write(&temporary, format!("{value}\n"))?;
    fs::rename(temporary, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{increment, LogicalState};
    use eidos_observe::StudyKey;

    fn token(value: u64) -> eidos_observe::ObjectToken {
        StudyKey::from_bytes([9; 32]).token("test", &value.to_le_bytes())
    }

    #[test]
    fn logical_accounting_maps_remain_bounded() {
        let mut state = LogicalState::new(4);
        for value in 0..100 {
            assert_eq!(increment(&mut state.edits, token(value)), 1);
            assert!(state.edits.len() <= 4);
        }
        assert!(!state.edits.contains(&token(0)));
        assert!(state.edits.contains(&token(99)));
        assert_eq!(increment(&mut state.edits, token(99)), 2);
    }
}
