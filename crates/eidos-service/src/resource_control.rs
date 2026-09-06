//! Explicit admission ceilings, not performance-qualified presets. Existing
//! scans/files finish at their original width; new work takes the saved limits.

use crate::api::{blocking, ApiError, ApiResult};
use crate::api_json::ApiJson;
use crate::background_probe::BackgroundProbe;
use crate::state::AppState;
use axum::{extract::State, routing::get, Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ts_rs::TS;

pub const SETTINGS_FILE: &str = "resource-limits.json";
const DISK_REFRESH: Duration = Duration::from_secs(5);
const DISK_MAX_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub scan_threads: u32,
    pub concurrent_scans: u32,
    /// Free space reserved on the data directory's filesystem, in MiB.
    /// Zero explicitly disables this admission check (not recommended).
    pub minimum_free_mib: u32,
}

impl ResourceLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=64).contains(&self.scan_threads) || !(1..=16).contains(&self.concurrent_scans) {
            return Err("scan threads must be 1..64 and concurrent scans must be 1..16");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, TS)]
pub struct ResourceView {
    pub limits: ResourceLimits,
    pub active_scans: u32,
    pub free_bytes: Option<u64>,
    pub disk_sample_age_s: Option<u64>,
    pub admission_blocked: Option<String>,
}

pub struct ResourceControl {
    data_dir: PathBuf,
    state: Mutex<(ResourceLimits, u32)>,
    /// Serialize persistence without holding the hot admission/status lock
    /// across filesystem I/O (especially fsync on a pressured data volume).
    settings_write: Mutex<()>,
    disk: Arc<BackgroundProbe<u64>>,
}

pub struct ScanReservation {
    control: Arc<ResourceControl>,
    pub threads: usize,
}

impl Drop for ScanReservation {
    fn drop(&mut self) {
        self.control.state.lock().1 -= 1;
    }
}

impl ResourceControl {
    pub fn load(data_dir: &Path, scan_threads: usize) -> anyhow::Result<Self> {
        let path = data_dir.join(SETTINGS_FILE);
        let limits = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("invalid {}: {e}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ResourceLimits {
                scan_threads: scan_threads.clamp(1, 64) as u32,
                concurrent_scans: 1,
                minimum_free_mib: 1024,
            },
            Err(error) => return Err(error.into()),
        };
        limits.validate().map_err(anyhow::Error::msg)?;
        // Startup already opens the local durable stores. Later refreshes
        // never run on the coordinator or on an HTTP runtime thread.
        let disk =
            BackgroundProbe::seeded(fs4::available_space(data_dir).map_err(|e| e.to_string()));
        Ok(Self {
            data_dir: data_dir.into(),
            state: Mutex::new((limits, 0)),
            settings_write: Mutex::new(()),
            disk: Arc::new(disk),
        })
    }

    pub fn set(&self, limits: ResourceLimits) -> anyhow::Result<()> {
        limits.validate().map_err(anyhow::Error::msg)?;
        let _write = self.settings_write.lock();
        let tmp = self.data_dir.join(format!("{SETTINGS_FILE}.tmp"));
        let replace = || -> anyhow::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&serde_json::to_vec(&limits)?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, self.data_dir.join(SETTINGS_FILE))?;
            Ok(())
        };
        if let Err(error) = replace() {
            // Never leave a half-written temporary behind for the next save
            // (or an operator reading the data directory) to trip over.
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        self.state.lock().0 = limits;
        Ok(())
    }

    pub fn refresh_disk(&self) {
        let path = self.data_dir.clone();
        self.disk.refresh(DISK_REFRESH, move || {
            fs4::available_space(&path).map_err(|e| e.to_string())
        });
    }

    fn disk_reason(
        limits: ResourceLimits,
        sample: &Option<(Duration, Result<u64, String>)>,
    ) -> Option<String> {
        if limits.minimum_free_mib == 0 {
            return None;
        }
        match sample {
            None => Some("checking free space on the data volume".into()),
            Some((age, _)) if *age > DISK_MAX_AGE => Some("data-volume free-space probe is stale; new work is held until it recovers".into()),
            Some((_, Err(error))) => Some(format!("cannot check data-volume free space: {error}")),
            Some((_, Ok(bytes))) if *bytes < u64::from(limits.minimum_free_mib) * 1024 * 1024 =>
                Some(format!("data volume below the {} MiB free-space reserve; free space or lower the reserve", limits.minimum_free_mib)),
            _ => None,
        }
    }

    pub fn blocked_reason(&self) -> Option<String> {
        Self::disk_reason(self.state.lock().0, &self.disk.snapshot())
    }

    pub fn view(&self) -> ResourceView {
        let (limits, active_scans) = *self.state.lock();
        let sample = self.disk.snapshot();
        ResourceView {
            limits,
            active_scans,
            free_bytes: sample
                .as_ref()
                .and_then(|(_, result)| result.as_ref().ok().copied()),
            disk_sample_age_s: sample.as_ref().map(|(age, _)| age.as_secs()),
            admission_blocked: Self::disk_reason(limits, &sample),
        }
    }

    pub fn try_scan(self: &Arc<Self>) -> Result<ScanReservation, String> {
        let mut state = self.state.lock();
        if let Some(reason) = Self::disk_reason(state.0, &self.disk.snapshot()) {
            return Err(reason);
        }
        if state.1 >= state.0.concurrent_scans {
            return Err("waiting for a metadata scan slot".into());
        }
        state.1 += 1;
        Ok(ScanReservation {
            control: self.clone(),
            threads: state.0.scan_threads as usize,
        })
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/resources", get(status).post(set_limits))
}

async fn status(State(st): State<Arc<AppState>>) -> ApiResult<ResourceView> {
    st.resources.refresh_disk();
    Ok(ApiJson(st.resources.view()))
}

async fn set_limits(
    State(st): State<Arc<AppState>>,
    Json(limits): Json<ResourceLimits>,
) -> ApiResult<ResourceView> {
    limits.validate().map_err(ApiError::bad_request)?;
    blocking(move || {
        st.resources
            .set(limits)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(ApiJson(st.resources.view()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_durable_and_lowering_does_not_revoke_active_work() {
        let dir = tempfile::tempdir().unwrap();
        let c = Arc::new(ResourceControl::load(dir.path(), 8).unwrap());
        let limits = ResourceLimits {
            scan_threads: 3,
            concurrent_scans: 2,
            minimum_free_mib: 0,
        };
        c.set(limits).unwrap();
        let a = c.try_scan().unwrap();
        let b = c.try_scan().unwrap();
        assert_eq!(a.threads, 3);
        c.set(ResourceLimits {
            concurrent_scans: 1,
            scan_threads: 1,
            ..limits
        })
        .unwrap();
        assert!(c.try_scan().is_err());
        drop(a);
        assert!(c.try_scan().is_err());
        drop(b);
        assert_eq!(c.try_scan().unwrap().threads, 1);
        assert_eq!(c.view().active_scans, 0);
        assert_eq!(
            ResourceControl::load(dir.path(), 8).unwrap().view().limits,
            c.view().limits
        );
    }

    #[test]
    fn failed_write_and_invalid_settings_do_not_change_effective_limits() {
        let dir = tempfile::tempdir().unwrap();
        let c = ResourceControl::load(dir.path(), 8).unwrap();
        let before = c.view().limits;
        assert!(c
            .set(ResourceLimits {
                scan_threads: 0,
                ..before
            })
            .is_err());
        std::fs::create_dir(dir.path().join("resource-limits.json.tmp")).unwrap();
        assert!(c
            .set(ResourceLimits {
                scan_threads: 1,
                ..before
            })
            .is_err());
        assert_eq!(c.view().limits, before);
        std::fs::write(dir.path().join(SETTINGS_FILE), b"invalid").unwrap();
        assert!(ResourceControl::load(dir.path(), 8).is_err());
    }

    #[test]
    fn low_space_errors_and_stale_samples_hold_new_work_and_recover() {
        let limits = ResourceLimits {
            scan_threads: 1,
            concurrent_scans: 1,
            minimum_free_mib: 1,
        };
        let reason = |sample| ResourceControl::disk_reason(limits, &sample);
        assert!(reason(None).is_some());
        assert!(reason(Some((Duration::ZERO, Ok(100))))
            .unwrap()
            .contains("reserve"));
        assert!(reason(Some((Duration::ZERO, Err("offline".into()))))
            .unwrap()
            .contains("offline"));
        assert!(reason(Some((Duration::from_secs(31), Ok(u64::MAX))))
            .unwrap()
            .contains("stale"));
        assert!(reason(Some((Duration::ZERO, Ok(1024 * 1024)))).is_none());
    }
}
