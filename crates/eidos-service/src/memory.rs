//! Demand-driven process memory sampling, independent of source/catalog I/O.
//! A single-flight probe serves cached results while refreshing every five
//! seconds; no polling task runs when nobody requests the diagnostics.

use crate::{
    api::ApiResult, api_json::ApiJson, background_probe::BackgroundProbe, state::AppState,
};
use axum::{extract::State, routing::get, Router};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use ts_rs::TS;

const REFRESH: Duration = Duration::from_secs(5);
const MAX_AGE: Duration = Duration::from_secs(30);
/// A one-shot caller such as `eidos resources --memory` cannot poll, so a cold
/// or aged-out cache waits this long for the refresh the same request started.
/// A usable sample never waits, and the wait never starts a second probe.
const SAMPLE_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, TS)]
pub struct ProcessMemory {
    pub pid: u32,
    /// Current working set on Windows; resident set on macOS/Linux.
    pub resident_bytes: u64,
    /// OS-reported lifetime peak, if available on this platform.
    pub peak_resident_bytes: Option<u64>,
    /// Windows private committed bytes, NOT private resident RAM.
    pub private_commit_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, TS)]
pub struct MemoryView {
    pub process: Option<ProcessMemory>,
    pub sample_age_s: Option<u64>,
    pub stale: bool,
    pub error: Option<String>,
    pub catalog: eidos_catalog::CatalogMemoryConfig,
    pub catalog_writer_budget_bytes: u64,
    pub content_writer_budget_bytes: u64,
}

pub struct MemoryTelemetry {
    probe: Arc<BackgroundProbe<ProcessMemory>>,
}

impl Default for MemoryTelemetry {
    fn default() -> Self {
        Self {
            probe: Arc::new(BackgroundProbe::default()),
        }
    }
}

impl MemoryTelemetry {
    pub async fn view(&self, catalog: &eidos_catalog::Catalog) -> MemoryView {
        self.probe.refresh(REFRESH, sample_process);
        let _ = self
            .probe
            .cached_within(Some(MAX_AGE), SAMPLE_DEADLINE)
            .await;
        self.cached_view(catalog)
    }

    fn cached_view(&self, catalog: &eidos_catalog::Catalog) -> MemoryView {
        let sample = self.probe.snapshot();
        let mut view = MemoryView {
            process: None,
            sample_age_s: sample.as_ref().map(|(age, _)| age.as_secs()),
            stale: sample
                .as_ref()
                .is_none_or(|(age, result)| *age > MAX_AGE || result.is_err()),
            error: None,
            catalog: catalog.memory_config(),
            catalog_writer_budget_bytes: eidos_search::CATALOG_WRITER_MEMORY_BYTES as u64,
            content_writer_budget_bytes: eidos_search::content::CONTENT_WRITER_MEMORY_BYTES as u64,
        };
        if let Some((_, result)) = sample {
            match result {
                Ok(process) => view.process = Some(process),
                Err(error) => view.error = Some(error),
            }
        }
        view
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/memory", get(status))
}

async fn status(State(state): State<Arc<AppState>>) -> ApiResult<MemoryView> {
    Ok(ApiJson(state.memory.view(&state.catalog).await))
}

#[cfg(windows)]
fn sample_process() -> Result<ProcessMemory, String> {
    use windows_sys::Win32::System::{
        ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::GetCurrentProcess,
    };
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of_val(&counters) as u32;
    // SAFETY: current-process pseudo-handle is valid and needs no close. The
    // EX buffer is writable and its exact size is supplied, as required by PSAPI.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(ProcessMemory {
        pid: std::process::id(),
        resident_bytes: counters.WorkingSetSize as u64,
        peak_resident_bytes: Some(counters.PeakWorkingSetSize as u64),
        private_commit_bytes: Some(counters.PrivateUsage as u64),
    })
}

#[cfg(target_os = "macos")]
fn sample_process() -> Result<ProcessMemory, String> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    // SAFETY: PROC_PIDTASKINFO writes a proc_taskinfo into this correctly sized
    // buffer for our own PID. Reject partial or failed results.
    let read = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    if read != size {
        return Err(format!(
            "process memory probe returned {read}/{size} bytes: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(ProcessMemory {
        pid: std::process::id(),
        resident_bytes: info.pti_resident_size,
        peak_resident_bytes: None,
        private_commit_bytes: None,
    })
}

#[cfg(target_os = "linux")]
fn sample_process() -> Result<ProcessMemory, String> {
    use std::io::Read;
    let mut status = String::new();
    std::fs::File::open("/proc/self/status")
        .map_err(|e| e.to_string())?
        .take(16 * 1024)
        .read_to_string(&mut status)
        .map_err(|e| e.to_string())?;
    parse_linux_status(&status)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_status(status: &str) -> Result<ProcessMemory, String> {
    let value = |name: &str| -> Option<u64> {
        let line = status.lines().find_map(|line| line.strip_prefix(name))?;
        let mut fields = line.split_whitespace();
        let kib: u64 = fields.next()?.parse().ok()?;
        if fields.next()? != "kB" {
            return None;
        }
        kib.checked_mul(1024)
    };
    Ok(ProcessMemory {
        pid: std::process::id(),
        resident_bytes: value("VmRSS:")
            .ok_or("resident memory is unavailable in /proc/self/status")?,
        peak_resident_bytes: value("VmHWM:"),
        private_commit_bytes: None,
    })
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn sample_process() -> Result<ProcessMemory, String> {
    Err("process memory is not available on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_units_and_missing_values_are_not_confused_with_zero() {
        let sample = parse_linux_status("VmRSS: 1234 kB\nVmHWM: 5678 kB\n").unwrap();
        assert_eq!(sample.resident_bytes, 1234 * 1024);
        assert_eq!(sample.peak_resident_bytes, Some(5678 * 1024));
        assert!(sample.private_commit_bytes.is_none());
        assert!(parse_linux_status("VmRSS: 42 MB").is_err());
        assert!(parse_linux_status("VmRSS: 18446744073709551615 kB").is_err());
        assert!(parse_linux_status("").is_err());
        assert!(parse_linux_status("VmRSS: 1 kB")
            .unwrap()
            .peak_resident_bytes
            .is_none());
    }

    #[test]
    fn unavailable_probe_preserves_configuration_without_taking_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = eidos_catalog::Catalog::open(dir.path().join("catalog.db")).unwrap();
        let telemetry = MemoryTelemetry {
            probe: Arc::new(BackgroundProbe::seeded(Err("probe failed".into()))),
        };
        let before = catalog.writer_stats().acquisitions;
        for _ in 0..100 {
            let view = telemetry.cached_view(&catalog);
            assert!(view.process.is_none());
            assert!(view.stale);
            assert_eq!(view.error.as_deref(), Some("probe failed"));
            assert_eq!(view.catalog.baseline_connections, 13);
            assert_eq!(
                view.catalog.page_cache_baseline_target_bytes,
                13 * 64 * 1024 * 1024
            );
        }
        assert_eq!(catalog.writer_stats().acquisitions, before);
    }

    #[test]
    fn reported_catalog_settings_match_an_actual_connection() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = eidos_catalog::Catalog::open(dir.path().join("catalog.db")).unwrap();
        let config = catalog.memory_config();
        catalog
            .with_reader(|conn| {
                let kib: i64 = conn.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
                let mmap: i64 = conn.query_row("PRAGMA mmap_size", [], |r| r.get(0))?;
                assert_eq!(config.page_cache_per_connection_bytes, (-kib as u64) * 1024);
                assert_eq!(config.mmap_per_connection_limit_bytes, mmap as u64);
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_recent_probe_failure_answers_without_waiting_for_a_new_sample() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = eidos_catalog::Catalog::open(dir.path().join("catalog.db")).unwrap();
        let telemetry = MemoryTelemetry {
            probe: Arc::new(BackgroundProbe::seeded(Err("probe failed".into()))),
        };
        // A successful sample here would mean the request blocked on a probe it
        // should not have started: the failure is inside the refresh interval.
        let view = telemetry.view(&catalog).await;
        assert!(view.process.is_none());
        assert!(view.stale);
        assert_eq!(view.error.as_deref(), Some("probe failed"));
    }

    #[tokio::test]
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    async fn a_cold_cache_answers_a_single_request_instead_of_asking_it_to_retry() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = eidos_catalog::Catalog::open(dir.path().join("catalog.db")).unwrap();
        let view = MemoryTelemetry::default().view(&catalog).await;
        let process = view.process.expect("a cold cache must wait for its sample");
        assert_eq!(process.pid, std::process::id());
        assert!(!view.stale);
        assert_eq!(view.sample_age_s, Some(0));
    }

    #[tokio::test]
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    async fn a_sample_aged_out_by_idleness_is_replaced_before_answering() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = eidos_catalog::Catalog::open(dir.path().join("catalog.db")).unwrap();
        let telemetry = MemoryTelemetry {
            probe: Arc::new(BackgroundProbe::seeded_at(
                Ok(ProcessMemory {
                    pid: 0,
                    resident_bytes: 1,
                    peak_resident_bytes: None,
                    private_commit_bytes: None,
                }),
                MAX_AGE + Duration::from_secs(1),
            )),
        };
        let view = telemetry.view(&catalog).await;
        let process = view.process.expect("an aged sample must be refreshed");
        assert_eq!(process.pid, std::process::id());
        assert!(process.resident_bytes > 1);
        assert!(!view.stale);
        assert_eq!(view.sample_age_s, Some(0));
    }

    #[test]
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn actual_probe_reports_this_process() {
        let process = sample_process().unwrap();
        assert_eq!(process.pid, std::process::id());
        assert!(process.resident_bytes > 0);
        if let Some(peak) = process.peak_resident_bytes {
            assert!(peak >= process.resident_bytes);
        }
    }
}
