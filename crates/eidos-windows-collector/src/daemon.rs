//! The collector process: shared state, always-on L0 sampling (health,
//! resources, volume inventory, lifecycle), the control-pipe handler,
//! export staging, and lane supervision.

use crate::config::CollectorConfig;
use crate::hostfacts::{self, CpuSampler, HostFacts};
use crate::keystore;
use crate::pipe;
use crate::protocol::{
    CollectorStatus, EtwView, FeedView, ProcessView, Request, Response, SpoolView, VolumeView,
};
use crate::resources;
use crate::volumes::{self, VolumeFacts};
use eidos_observe::{
    bucket_age, bucket_capacity, bucket_size, export_bundle, BundleManifest, Capabilities,
    CaptureGap, DropCounters, EndpointSecurityCapability, EndpointSecurityState, EtwState,
    FeedState, GapCause, HealthRecord, HostResources, LaneStates, LifecycleEvent, MarkRecord,
    ObjectToken, ObservationRecord, ResourceSample, Spool, StudyKey, TimeAnchor, Units, UsnState,
    VolumeEvent, WindowsCapabilities, SCHEMA_VERSION,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    Stop,
    Shutdown,
    Suspend,
    Resume,
    PowerStatusChange,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub data_dir: PathBuf,
}

/// Per-volume feed status kept for `observe status`; the durable form is
/// `FeedHealthRecord`.
#[derive(Debug, Clone)]
pub struct FeedStatus {
    pub root: String,
    pub state: FeedState,
    pub detail: Option<String>,
    pub batches: u64,
    pub records: u64,
    pub logical_changes: u64,
    pub lag_bytes: u64,
    pub last_batch: Option<Instant>,
    pub overflows: u64,
    pub recreations: u64,
    /// Content nominations dropped because the probe queue was saturated.
    pub probe_dropped: u64,
}

impl FeedStatus {
    pub fn new(root: String) -> Self {
        Self {
            root,
            state: FeedState::Starting,
            detail: None,
            batches: 0,
            records: 0,
            logical_changes: 0,
            lag_bytes: 0,
            last_batch: None,
            overflows: 0,
            recreations: 0,
            probe_dropped: 0,
        }
    }
}

/// How many times a failed spool batch is retried before it is declared lost.
const SPOOL_RETRIES: u32 = 3;

pub struct Shared {
    pub data_dir: PathBuf,
    pub export_dir: PathBuf,
    /// Held for the duration of a staged export.
    pub export_lock: Mutex<()>,
    pub upload: Mutex<crate::protocol::UploadView>,
    pub key: Mutex<Option<StudyKey>>,
    pub spool: Mutex<Spool>,
    pub config: Mutex<CollectorConfig>,
    pub capabilities: Mutex<Capabilities>,
    pub drops: Mutex<DropCounters>,
    pub gaps: Mutex<Vec<CaptureGap>>,
    pub volumes: Mutex<Vec<VolumeFacts>>,
    pub feeds: Mutex<BTreeMap<String, FeedStatus>>,
    pub etw: Mutex<EtwView>,
    /// Candidates for the content probe, installed by the lane supervisor.
    pub content_tx: Mutex<Option<std::sync::mpsc::SyncSender<crate::content_probe::Candidate>>>,
    /// True while an ETW window is tracing; drives `LaneStates::etw`.
    pub etw_window_open: AtomicBool,
    /// None of the mutexes above may be held while another is taken, and
    /// none may be taken twice on one thread: they guard independent state,
    /// they are not reentrant, and the lanes reach for them in whatever order
    /// their work happens to need.
    ///
    /// Shared with the pipe server, which parks in a blocking accept and
    /// only leaves it when this flag is set and `pipe::poke` wakes it.
    pub shutdown: Arc<AtomicBool>,
    pub started: Instant,
    pub build_hash: String,
    pub facts: HostFacts,
}

impl Shared {
    /// Monotonic time is boot-relative (`GetTickCount64`), so anchors stay
    /// comparable across collector restarts within one boot.
    pub fn anchor(&self) -> TimeAnchor {
        TimeAnchor {
            monotonic_ns: hostfacts::uptime_ms().saturating_mul(1_000_000),
            utc_ns: utc_now_ns(),
        }
    }

    pub fn append(&self, record: ObservationRecord) {
        if let Err(error) = self.spool.lock().unwrap().append(&record) {
            tracing::error!(error = %error, "spool append failed");
            self.drops.lock().unwrap().user += 1;
        }
    }

    pub fn with_key<T>(&self, f: impl FnOnce(&StudyKey) -> T) -> Option<T> {
        self.key.lock().unwrap().as_ref().map(f)
    }

    pub fn token(&self, domain: &'static str, value: &[u8]) -> Option<ObjectToken> {
        self.with_key(|key| key.token(domain, value))
    }

    pub fn add_gap(&self, cause: GapCause, started_monotonic_ns: u64, estimated: Option<u64>) {
        let ended = self.anchor().monotonic_ns;
        self.gaps.lock().unwrap().push(CaptureGap {
            started_monotonic_ns,
            ended_monotonic_ns: ended,
            cause,
            estimated_events: estimated,
        });
        let mut drops = self.drops.lock().unwrap();
        match cause {
            GapCause::FeedOverflow | GapCause::JournalRecreated => drops.overflows += 1,
            GapCause::RootChanged => drops.root_changes += 1,
            GapCause::KernelDrop => drops.kernel += 1,
            _ => {}
        }
    }

    pub fn config_hash(&self) -> String {
        self.config.lock().unwrap().hash()
    }

    /// Lane states as they are in force now: the ETW flag is true only
    /// while a window is tracing, not merely when the lane is enabled.
    pub fn lanes(&self) -> LaneStates {
        let mut lanes = self.config.lock().unwrap().lane_states();
        lanes.etw = self.etw_window_open.load(Ordering::Acquire);
        lanes
    }

    pub fn lane_enabled(&self, lane: impl Fn(&CollectorConfig) -> bool) -> bool {
        lane(&self.config.lock().unwrap())
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Sleep in short slices so a stop is not delayed by a whole tick, and
    /// report whether the collector is stopping. Every supervisor loop waits
    /// this way: the service control manager gives a stop seconds, not
    /// minutes, and an installer replacing the service gives it less.
    pub fn sleep_unless_stopping(&self, total: Duration) -> bool {
        const SLICE: Duration = Duration::from_millis(250);
        let mut slept = Duration::ZERO;
        while slept < total {
            if self.is_shutting_down() {
                return true;
            }
            std::thread::sleep(SLICE);
            slept += SLICE;
        }
        self.is_shutting_down()
    }

    /// Append a batch, retrying briefly before reporting it lost.
    ///
    /// A lane that has already drained its journal batch or its aggregator
    /// cannot reproduce these records, and its cursor moves on regardless, so
    /// a transient spool error must not quietly discard them. Callers report
    /// the loss when this returns `false`.
    pub fn append_all_retrying(&self, records: &[ObservationRecord]) -> bool {
        for attempt in 1..=SPOOL_RETRIES {
            match self.spool.lock().unwrap().append_all(records) {
                Ok(()) => return true,
                Err(error) => {
                    tracing::error!(error = %error, attempt, "spool batch failed")
                }
            }
            if attempt == SPOOL_RETRIES {
                break;
            }
            for _ in 0..10 {
                if self.is_shutting_down() {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        false
    }

    fn health(&self, lifecycle: LifecycleEvent, clean_prior_shutdown: Option<bool>) {
        let process = resources::sample_process();
        self.append(ObservationRecord::Health(HealthRecord {
            at: self.anchor(),
            os_build: self.facts.os_build.clone(),
            machine: self.facts.machine,
            lifecycle,
            clean_prior_shutdown,
            feed_cursor: None,
            drops: self.drops.lock().unwrap().clone(),
            cpu_millis: process.cpu_ms,
            resident_bytes_bucket: bucket_size(process.working_set_bytes),
        }));
    }
}

pub fn utc_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

pub fn build_hash() -> String {
    blake3::hash(
        option_env!("EIDOS_BUILD_REVISION")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .as_bytes(),
    )
    .to_hex()
    .to_string()
}

pub fn run(
    options: Options,
    control: Receiver<ControlEvent>,
    on_ready: impl FnOnce(),
) -> anyhow::Result<()> {
    let data_dir = options.data_dir;
    let export_dir = data_dir.join("exports");
    std::fs::create_dir_all(&export_dir)?;

    let config = CollectorConfig::load(&data_dir)?;
    if !CollectorConfig::path(&data_dir).exists() {
        config.save(&data_dir)?;
    }
    let clean_marker = data_dir.join("clean-shutdown");
    let clean_prior_shutdown = clean_marker.exists();
    if clean_prior_shutdown {
        std::fs::remove_file(&clean_marker)?;
    }
    let build_hash = build_hash();
    let build_marker = data_dir.join("build.hash");
    let upgraded = match std::fs::read_to_string(&build_marker) {
        Ok(previous) => previous.trim() != build_hash,
        Err(_) => false,
    };
    std::fs::write(&build_marker, &build_hash)?;

    let spool = Spool::open(&data_dir.join("spool.db"), config.spool_limits())?;
    let key = match keystore::load(&data_dir) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(error = %error, "study key unusable; collecting health only");
            None
        }
    };
    let facts = hostfacts::host_facts();
    let inventory = volumes::enumerate();
    let capabilities = Capabilities {
        fsevents: false,
        endpoint_security: EndpointSecurityCapability {
            state: EndpointSecurityState::Off,
            entitlement_claimed: false,
            tcc_full_disk_access: None,
            running_as_root: facts.local_system,
        },
        apfs: false,
        windows: Some(WindowsCapabilities {
            usn: usn_state(&inventory),
            etw: EtwState::Off,
            running_as_system: facts.local_system,
            elevated: facts.elevated,
            study_key_available: key.is_some(),
        }),
    };
    let shared = Arc::new(Shared {
        data_dir: data_dir.clone(),
        export_dir,
        export_lock: Mutex::new(()),
        upload: Mutex::new(crate::protocol::UploadView::default()),
        key: Mutex::new(key),
        spool: Mutex::new(spool),
        config: Mutex::new(config),
        capabilities: Mutex::new(capabilities),
        drops: Mutex::new(DropCounters::default()),
        gaps: Mutex::new(Vec::new()),
        volumes: Mutex::new(Vec::new()),
        feeds: Mutex::new(BTreeMap::new()),
        etw: Mutex::new(EtwView {
            state: "off".into(),
            ..EtwView::default()
        }),
        content_tx: Mutex::new(None),
        etw_window_open: AtomicBool::new(false),
        shutdown: Arc::new(AtomicBool::new(false)),
        started: Instant::now(),
        build_hash,
        facts,
    });
    if !clean_prior_shutdown && std::fs::metadata(data_dir.join("spool.db")).is_ok() {
        // The previous incarnation died without its marker; its last
        // record is the best estimate of when capture stopped.
        let last = shared.spool.lock().unwrap().latest().ok().flatten();
        if let Some(last) = last {
            shared.add_gap(GapCause::UncleanShutdown, last.at().monotonic_ns, None);
        }
    }
    shared.health(LifecycleEvent::Started, Some(clean_prior_shutdown));
    if upgraded {
        shared.health(LifecycleEvent::Upgraded, None);
    }
    if shared.key.lock().unwrap().is_none() {
        shared.add_gap(GapCause::KeyUnavailable, shared.anchor().monotonic_ns, None);
    }
    apply_inventory(&shared, inventory, true);
    tracing::info!(
        os = %shared.facts.os_build,
        machine = ?shared.facts.machine,
        local_system = shared.facts.local_system,
        key = shared.key.lock().unwrap().is_some(),
        volumes = shared.volumes.lock().unwrap().len(),
        "collector started"
    );

    let pipe_thread = {
        // The server's flag is the daemon's own: a private one would leave
        // the accept loop parked forever and `join` below would never return.
        let shutdown = shared.shutdown.clone();
        let shared = shared.clone();
        pipe::serve(
            shutdown,
            Arc::new(move |request| handle_request(&shared, request)),
        )?
    };
    let health_thread = {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("collector-health".into())
            .spawn(move || run_health(shared))?
    };
    let lane_threads = crate::lanes::start(shared.clone());
    on_ready();

    loop {
        match control.recv_timeout(Duration::from_secs(1)) {
            Ok(ControlEvent::Stop) => {
                tracing::info!("stop requested");
                break;
            }
            Ok(ControlEvent::Shutdown) => {
                tracing::info!("system shutdown");
                shared.health(LifecycleEvent::Shutdown, None);
                break;
            }
            Ok(ControlEvent::Suspend) => shared.health(LifecycleEvent::Sleep, None),
            Ok(ControlEvent::Resume) => shared.health(LifecycleEvent::Wake, None),
            Ok(ControlEvent::PowerStatusChange) => {
                shared.health(LifecycleEvent::PowerStatusChange, None)
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    shared.shutdown.store(true, Ordering::Release);
    pipe::poke();
    for thread in lane_threads {
        let _ = thread.join();
    }
    let _ = health_thread.join();
    let _ = pipe_thread.join();
    shared.health(LifecycleEvent::Heartbeat, None);
    std::fs::write(clean_marker, b"clean\n")?;
    tracing::info!("collector stopped");
    Ok(())
}

fn usn_state(inventory: &[VolumeFacts]) -> UsnState {
    let candidates: Vec<&VolumeFacts> =
        inventory.iter().filter(|v| v.is_feed_candidate()).collect();
    if candidates.is_empty() {
        return if inventory.iter().any(|v| v.supports_usn()) {
            UsnState::NoJournaledVolume
        } else {
            UsnState::Unsupported
        };
    }
    if candidates.iter().any(|v| v.journal.is_some()) {
        UsnState::Available
    } else if candidates.iter().any(|v| {
        v.journal_error
            .as_deref()
            .is_some_and(|e| e.contains("access denied"))
    }) {
        UsnState::AccessDenied
    } else {
        UsnState::NoJournaledVolume
    }
}

/// Record inventory changes and replace the cached volume list.
fn apply_inventory(shared: &Shared, inventory: Vec<VolumeFacts>, initial: bool) {
    let events = if initial {
        inventory
            .iter()
            .map(|v| (VolumeEvent::Inventory, v.clone()))
            .collect()
    } else {
        volumes::diff(&shared.volumes.lock().unwrap(), &inventory)
    };
    let excluded = shared.config.lock().unwrap().exclude_volumes.clone();
    for (event, volume) in events {
        if excluded.iter().any(|e| volume.matches_exclusion(e)) {
            continue;
        }
        let at = shared.anchor();
        if let Some(record) = shared.with_key(|key| volume.observation(key, event, at)) {
            shared.append(ObservationRecord::Volume(record));
        }
        match event {
            VolumeEvent::Mounted => shared.health(LifecycleEvent::Mounted, None),
            VolumeEvent::Unmounted => shared.health(LifecycleEvent::Unmounted, None),
            _ => {}
        }
    }
    if let Some(windows) = shared.capabilities.lock().unwrap().windows.as_mut() {
        windows.usn = usn_state(&inventory);
    }
    *shared.volumes.lock().unwrap() = inventory;
}

fn run_health(shared: Arc<Shared>) {
    let mut cpu = CpuSampler::new();
    let mut last_heartbeat = Instant::now();
    let mut last_resource = Instant::now();
    let mut last_volume_scan = Instant::now();
    let mut last_slept_ms = hostfacts::slept_ms();
    let mut last_wall = (Instant::now(), utc_now_ns());
    let mut last_cpu_ms = resources::sample_process().cpu_ms;
    while !shared.is_shutting_down() {
        std::thread::sleep(Duration::from_secs(1));
        // One at a time: the USN reader holds `key` across a batch and
        // needs `config` inside it, so holding `config` here while reaching
        // for `key` deadlocks both threads.
        let intervals = shared.config.lock().unwrap().intervals.clone();
        let key_missing = shared.key.lock().unwrap().is_none();

        // Wall clock versus monotonic drift beyond five seconds is a clock
        // jump (NTP step, manual change, or resume without a power event).
        let now_wall = utc_now_ns();
        let expected = last_wall.1 + last_wall.0.elapsed().as_nanos() as i64;
        if (now_wall - expected).abs() > 5_000_000_000 {
            let slept = hostfacts::slept_ms();
            if slept > last_slept_ms {
                tracing::info!(slept_ms = slept - last_slept_ms, "resume detected");
                shared.health(LifecycleEvent::Wake, None);
            } else {
                tracing::info!(delta_ms = (now_wall - expected) / 1_000_000, "clock jump");
                shared.add_gap(GapCause::ClockJump, shared.anchor().monotonic_ns, None);
                shared.health(LifecycleEvent::ClockJump, None);
            }
            last_slept_ms = slept;
        }
        last_wall = (Instant::now(), now_wall);

        if key_missing {
            if let Ok(Some(key)) = keystore::load(&shared.data_dir) {
                tracing::info!("study key became available");
                *shared.key.lock().unwrap() = Some(key);
                if let Some(windows) = shared.capabilities.lock().unwrap().windows.as_mut() {
                    windows.study_key_available = true;
                }
                let inventory = shared.volumes.lock().unwrap().clone();
                apply_inventory(&shared, inventory, true);
            }
        }
        if last_volume_scan.elapsed() >= Duration::from_secs(intervals.volume_scan_s.max(5) as u64)
        {
            last_volume_scan = Instant::now();
            apply_inventory(&shared, volumes::enumerate(), false);
        }
        if last_resource.elapsed() >= Duration::from_secs(intervals.resource_s.max(10) as u64) {
            let interval_s = last_resource.elapsed().as_secs() as u32;
            last_resource = Instant::now();
            let mut process = resources::sample_process();
            let total_cpu = process.cpu_ms;
            process.cpu_ms = total_cpu.saturating_sub(last_cpu_ms);
            last_cpu_ms = total_cpu;
            let (memory_total, memory_used_percent) = hostfacts::memory();
            shared.append(ObservationRecord::Resource(ResourceSample {
                at: shared.anchor(),
                interval_s,
                collector: process,
                system: HostResources {
                    cpu_busy_percent: cpu.busy_percent(),
                    memory_used_percent,
                    memory_total: bucket_capacity(memory_total),
                    logical_processors: shared.facts.logical_processors,
                    uptime: bucket_age(hostfacts::uptime_ms() / 1_000),
                    on_battery: hostfacts::on_battery(),
                    slept_ms: hostfacts::slept_ms(),
                },
                lanes: shared.lanes(),
            }));
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(intervals.heartbeat_s.max(30) as u64) {
            last_heartbeat = Instant::now();
            shared.health(LifecycleEvent::Heartbeat, None);
        }
    }
}

fn handle_request(shared: &Arc<Shared>, request: Request) -> Response {
    match request {
        Request::Status => Response::Status {
            status: Box::new(status(shared)),
        },
        Request::Mark { label } => match shared.token("mark", label.as_bytes()) {
            Some(marker) => {
                shared.append(ObservationRecord::Mark(MarkRecord {
                    at: shared.anchor(),
                    marker,
                }));
                Response::Accepted
            }
            None => Response::Error {
                message: "no study key; run `eidos observe init` first".into(),
            },
        },
        Request::Export => match stage_export(shared) {
            Ok(staged_file) => Response::Exported { staged_file },
            Err(error) => Response::Error {
                message: format!("export failed: {error:#}"),
            },
        },
        Request::SetLanes {
            usn,
            etw,
            content,
            enumeration,
        } => {
            let mut config = shared.config.lock().unwrap();
            if let Some(usn) = usn {
                config.lanes.usn = usn;
            }
            if let Some(etw) = etw {
                config.lanes.etw.enabled = etw;
            }
            if let Some(content) = content {
                config.lanes.content.enabled = content;
            }
            if let Some(enumeration) = enumeration {
                config.lanes.enumeration.enabled = enumeration;
            }
            match config.save(&shared.data_dir) {
                Ok(()) => Response::Accepted,
                Err(error) => Response::Error {
                    message: format!("configuration not saved: {error}"),
                },
            }
        }
        Request::Probe { volume } => crate::lanes::probe_now(shared, volume.as_deref()),
    }
}

fn status(shared: &Shared) -> CollectorStatus {
    let spool = shared.spool.lock().unwrap();
    let stats = spool.stats().ok();
    drop(spool);
    let file_bytes = ["spool.db", "spool.db-wal"]
        .iter()
        .filter_map(|name| std::fs::metadata(shared.data_dir.join(name)).ok())
        .map(|m| m.len())
        .sum();
    let config = shared.config.lock().unwrap();
    let excluded = config.exclude_volumes.clone();
    let lanes = config.lane_states();
    let config_hash = config.hash();
    drop(config);
    let feeds = shared.feeds.lock().unwrap();
    let process = resources::sample_process();
    CollectorStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        build_hash: shared.build_hash.clone(),
        config_hash,
        uptime_s: shared.started.elapsed().as_secs(),
        capabilities: shared.capabilities.lock().unwrap().clone(),
        lanes,
        spool: SpoolView {
            records: stats.map(|s| s.records).unwrap_or(0),
            detailed_records: stats.map(|s| s.detailed_records).unwrap_or(0),
            detailed_bytes: stats.map(|s| s.detailed_bytes).unwrap_or(0),
            oldest_utc_ns: stats.and_then(|s| s.oldest_utc_ns),
            newest_utc_ns: stats.and_then(|s| s.newest_utc_ns),
            file_bytes,
        },
        drops: shared.drops.lock().unwrap().clone(),
        capture_gaps: shared.gaps.lock().unwrap().len(),
        volumes: shared
            .volumes
            .lock()
            .unwrap()
            .iter()
            .map(|v| VolumeView {
                root: v.root().to_string(),
                filesystem: v.filesystem_name.clone(),
                drive: format!("{:?}", v.drive).to_ascii_lowercase(),
                bus: format!("{:?}", v.bus).to_ascii_lowercase(),
                media: format!("{:?}", v.media).to_ascii_lowercase(),
                journaled: v.journal.is_some(),
                excluded: excluded.iter().any(|e| v.matches_exclusion(e)),
            })
            .collect(),
        feeds: feeds
            .values()
            .map(|f| FeedView {
                root: f.root.clone(),
                state: format!("{:?}", f.state).to_ascii_lowercase(),
                detail: f.detail.clone(),
                batches: f.batches,
                records: f.records,
                logical_changes: f.logical_changes,
                lag_bytes: f.lag_bytes,
                last_batch_s_ago: f.last_batch.map(|t| t.elapsed().as_secs()),
                overflows: f.overflows,
                recreations: f.recreations,
                probe_dropped: f.probe_dropped,
            })
            .collect(),
        etw: shared.etw.lock().unwrap().clone(),
        upload: shared.upload.lock().unwrap().clone(),
        collector: ProcessView {
            cpu_ms: process.cpu_ms,
            working_set_bytes: process.working_set_bytes,
            private_bytes: process.private_bytes,
            handles: process.handles,
            threads: process.threads,
        },
    }
}

/// Distinguishes exports staged within the same second.
static EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn stage_export(shared: &Shared) -> anyhow::Result<PathBuf> {
    // One export at a time: concurrent requests otherwise duplicate a full
    // pass over the ring, and used to race for the same staged path.
    let _staging = shared.export_lock.lock().unwrap();
    let manifest = BundleManifest {
        schema: SCHEMA_VERSION.into(),
        build_hash: shared.build_hash.clone(),
        config_hash: shared.config_hash(),
        created: shared.anchor(),
        capabilities: shared.capabilities.lock().unwrap().clone(),
        capture_gaps: shared.gaps.lock().unwrap().clone(),
        drops: shared.drops.lock().unwrap().clone(),
        units: Units::default(),
    };
    // The sequence keeps two exports in the same wall-clock second from
    // deriving one path, where the second silently replaced the first.
    let file = shared.export_dir.join(format!(
        "observation-{}-{:04}.eidos-observation.zst",
        utc_now_ns() / 1_000_000_000,
        EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed) % 10_000
    ));
    // Stream straight off the ring on a read-only connection: the export
    // neither takes the append lock nor holds the spool in memory, so a
    // multi-gigabyte ring cannot stall or exhaust the collector.
    let records = export_bundle(&shared.data_dir.join("spool.db"), &manifest, &file)?;
    tracing::info!(records, file = %file.display(), "export staged");
    Ok(file)
}

pub fn export_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("exports")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Another collector already owns the machine-wide control pipe, so this
    /// process cannot bind it. Listing the pipe filesystem answers that
    /// without connecting to whoever is serving.
    fn pipe_already_served() -> bool {
        std::fs::read_dir(r"\\.\pipe\")
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name() == crate::SERVICE_NAME)
            })
            .unwrap_or(false)
    }

    /// A working collector must return from `run` within seconds of a Stop.
    ///
    /// Everything about a stop is on a clock somebody else holds: the service
    /// control manager gives it seconds, and an installer replacing the
    /// service gives it less before failing with error 1921 and rolling the
    /// upgrade back. Three ways to lose that race have been paid for already,
    /// and none of them shows up in a collector that is asked to stop the
    /// instant it starts:
    ///
    /// - the pipe server parking forever in its accept, because it was handed
    ///   a shutdown flag that nothing sets;
    /// - supervisor loops that only look at the flag once a tick;
    /// - the USN reader taking the study key a second time on its own thread,
    ///   through the content lane's sampler, on the first create it nominates.
    ///
    /// So this test gives the collector a key, turns the content lane on,
    /// makes some churn for the reader to analyse, and lets every thread
    /// settle into its own work before asking for a stop. The churn is
    /// best-effort - a host that will not open its journal simply has less to
    /// analyse - but the promptness is not.
    ///
    /// A stop that never finishes also never writes the clean-shutdown
    /// marker, so the next start declares a capture gap that never happened.
    #[test]
    fn a_working_collector_stops_promptly_and_marks_a_clean_shutdown() {
        let _guard = crate::pipe::test_lock();
        if pipe_already_served() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("collector");
        let churn_dir = temp.path().join("churn");
        std::fs::create_dir_all(&churn_dir).unwrap();
        crate::keystore::create(&data_dir, Some([7u8; 32]), true).unwrap();
        let mut config = crate::config::CollectorConfig::default();
        config.lanes.content.enabled = true;
        // Under 100, so the sampler actually runs for every nomination.
        config.lanes.content.sample_percent = 50;
        config.save(&data_dir).unwrap();

        let marker = data_dir.join("clean-shutdown");
        let (control_tx, control_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let running = data_dir.clone();
        let daemon = std::thread::spawn(move || {
            run(Options { data_dir: running }, control_rx, move || {
                let _ = ready_tx.send(());
            })
        });
        // Either it comes up, or it fails fast (another collector owns the
        // pipe after all): waiting the whole minute for a thread that has
        // already returned an error tells nobody anything.
        let ready_by = Instant::now() + Duration::from_secs(60);
        while ready_rx.try_recv().is_err() {
            if daemon.is_finished() {
                daemon
                    .join()
                    .unwrap()
                    .expect("the collector failed to start");
                panic!("the collector returned before it was ready");
            }
            assert!(
                Instant::now() < ready_by,
                "the collector never reported itself running"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        for index in 0..64 {
            let _ = std::fs::write(churn_dir.join(format!("{index}.txt")), b"observed change");
        }
        // Long enough for the reader to analyse that churn and for every
        // supervisor to be inside a tick rather than at the top of its loop.
        std::thread::sleep(Duration::from_secs(3));
        // Off-thread and bounded: the defects this test exists for wedge the
        // threads a status request has to wait on - the health thread holds
        // `config` while it blocks, and building a status takes `config` -
        // so asking for one on this thread would hang the test instead of
        // failing it.
        let answered = {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::client::request(&crate::protocol::Request::Status).is_ok());
            });
            rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false)
        };
        assert!(
            answered,
            "the control pipe must answer before the stop is asked for"
        );

        let asked = Instant::now();
        control_tx.send(ControlEvent::Stop).unwrap();
        let deadline = asked + Duration::from_secs(10);
        while !daemon.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            daemon.is_finished(),
            "run did not return within 10s of Stop ({}s and counting); the SCM would give up on the service first",
            asked.elapsed().as_secs()
        );
        daemon.join().unwrap().unwrap();
        assert!(
            marker.exists(),
            "a clean stop must leave the marker that keeps the next start from declaring a gap"
        );
    }
}
