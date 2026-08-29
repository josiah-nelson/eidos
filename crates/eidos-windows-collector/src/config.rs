//! Collector configuration: a JSON file in the data directory, written by
//! `observe init`, adjusted at runtime through the control pipe, and hashed
//! into every bundle manifest so exports say which lanes produced them.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(windows)]
const CONFIG_LOCK_FILE: &str = ".config.lock";

pub const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorConfig {
    pub lanes: LaneConfig,
    pub intervals: Intervals,
    pub spool: SpoolConfig,
    /// Drive letters or volume GUID paths to leave alone entirely. Local
    /// only; never exported.
    pub exclude_volumes: Vec<String>,
    /// Where and when finished bundles are delivered.
    pub upload: UploadConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadConfig {
    pub enabled: bool,
    /// Directory the daily bundle is copied into — typically a UNC share such
    /// as `\\fileserver\share\eidos`. The collector runs as LocalSystem, so a
    /// share must grant write access to the machine account rather than to the
    /// operator who configured it. Local only; never exported.
    pub destination: String,
    /// Local hour, 0-23, at or after which the day's upload runs.
    pub hour: u32,
    /// Attempts per day before leaving it until tomorrow. Bundles that were
    /// not delivered stay staged and are retried on the next run.
    pub attempts: u32,
    /// Delete the staged bundle once it has been delivered.
    pub remove_after_upload: bool,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: String::new(),
            // Off-hours by default, so a fleet is not copying bundles while
            // the workload it measures is busiest.
            hour: 3,
            attempts: 3,
            remove_after_upload: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneConfig {
    /// L1: USN journal change traces on every journaled local volume.
    pub usn: bool,
    /// L2: ETW file-access lane, run in randomized windows.
    pub etw: EtwLane,
    /// L2: content economics probe on files closed after a write.
    pub content: ContentLane,
    /// L2: periodic read-only enumeration of each fixed volume.
    pub enumeration: EnumerationLane,
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            usn: true,
            etw: EtwLane::default(),
            content: ContentLane::default(),
            enumeration: EnumerationLane::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EtwLane {
    pub enabled: bool,
    /// Minutes of tracing per hour, placed at a random offset so the
    /// observed workload cannot align with the window.
    pub minutes_per_hour: u32,
}

impl Default for EtwLane {
    fn default() -> Self {
        Self {
            enabled: false,
            minutes_per_hour: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentLane {
    pub enabled: bool,
    /// Percentage of eligible closed-after-write files that are measured.
    pub sample_percent: u8,
    pub max_bytes: u64,
    /// Upper bound on bytes read per hour across all sampled files.
    pub hourly_budget_bytes: u64,
}

impl Default for ContentLane {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_percent: 10,
            max_bytes: 64 * 1024 * 1024,
            hourly_budget_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnumerationLane {
    pub enabled: bool,
    pub every_hours: u32,
}

impl Default for EnumerationLane {
    fn default() -> Self {
        Self {
            enabled: false,
            every_hours: 24,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Intervals {
    pub heartbeat_s: u32,
    pub resource_s: u32,
    pub rate_s: u32,
    pub volume_scan_s: u32,
    pub feed_health_s: u32,
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            heartbeat_s: 300,
            resource_s: 60,
            rate_s: 60,
            volume_scan_s: 30,
            feed_health_s: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpoolConfig {
    pub detailed_max_bytes: u64,
    pub detailed_days: u32,
    pub summary_days: u32,
}

impl Default for SpoolConfig {
    fn default() -> Self {
        Self {
            detailed_max_bytes: 10 * 1024 * 1024 * 1024,
            detailed_days: 14,
            summary_days: 90,
        }
    }
}

impl CollectorConfig {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(CONFIG_FILE)
    }

    /// Missing file means defaults; a malformed file is an error rather
    /// than a silent reset, because it would change what gets collected.
    pub fn load(data_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(data_dir);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let path = Self::path(data_dir);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    /// Atomically perform a read-modify-write against every collector process
    /// using this data directory. `observe configure` and the daemon's lane
    /// handler run in different processes, so the daemon's in-memory mutex is
    /// not enough to keep one from saving over the other's update.
    #[cfg(windows)]
    pub fn edit_locked(
        data_dir: &Path,
        edit: impl FnOnce(&mut Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let _lock = ConfigLock::acquire(data_dir)?;
        let mut config = Self::load(data_dir)?;
        edit(&mut config)?;
        config.save(data_dir)?;
        Ok(config)
    }

    /// Stable hash of the collection-affecting configuration.
    ///
    /// Upload settings are deliberately excluded: where a bundle is delivered
    /// says nothing about how it was collected, and pointing the collector at
    /// a different share should not make every later bundle look as though it
    /// came from a different collection configuration.
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_vec(&(
            &self.lanes,
            &self.intervals,
            &self.spool,
            &self.exclude_volumes,
        ))
        .unwrap_or_default();
        blake3::hash(&canonical).to_hex().to_string()
    }

    pub fn spool_limits(&self) -> eidos_observe::SpoolLimits {
        const DAY_NS: i64 = 86_400_000_000_000;
        eidos_observe::SpoolLimits {
            detailed_max_bytes: self.spool.detailed_max_bytes,
            detailed_max_age_ns: DAY_NS.saturating_mul(self.spool.detailed_days as i64),
            summary_max_age_ns: DAY_NS.saturating_mul(self.spool.summary_days as i64),
        }
    }

    pub fn lane_states(&self) -> eidos_observe::LaneStates {
        eidos_observe::LaneStates {
            usn: self.lanes.usn,
            etw: self.lanes.etw.enabled,
            content_probe: self.lanes.content.enabled,
            enumeration_probe: self.lanes.enumeration.enabled,
        }
    }
}

#[cfg(windows)]
struct ConfigLock {
    // Closing the handle releases every byte-range lock it owns, including
    // during unwinding or process termination.
    _file: std::fs::File,
}

#[cfg(windows)]
impl ConfigLock {
    fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
        };
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let path = data_dir.join(CONFIG_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .with_context(|| format!("opening configuration lock {}", path.display()))?;
        let mut overlapped = OVERLAPPED::default();
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if locked == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking configuration {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_missing_fields() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            CollectorConfig::load(temp.path()).unwrap(),
            CollectorConfig::default()
        );
        let mut config = CollectorConfig::default();
        config.lanes.etw.enabled = true;
        config.save(temp.path()).unwrap();
        assert_eq!(CollectorConfig::load(temp.path()).unwrap(), config);
        assert_ne!(config.hash(), CollectorConfig::default().hash());

        std::fs::write(
            CollectorConfig::path(temp.path()),
            br#"{"lanes":{"usn":false}}"#,
        )
        .unwrap();
        let partial = CollectorConfig::load(temp.path()).unwrap();
        assert!(!partial.lanes.usn);
        assert_eq!(partial.intervals, Intervals::default());

        std::fs::write(CollectorConfig::path(temp.path()), b"{not json").unwrap();
        assert!(CollectorConfig::load(temp.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn locked_edits_preserve_concurrent_changes() {
        use std::sync::mpsc;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        CollectorConfig::default().save(temp.path()).unwrap();
        let first_dir = temp.path().to_path_buf();
        let second_dir = first_dir.clone();
        let (first_locked_tx, first_locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = std::thread::spawn(move || {
            CollectorConfig::edit_locked(&first_dir, |config| {
                config.upload.destination = r"\\fileserver\share\eidos".into();
                first_locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        first_locked_rx.recv().unwrap();

        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            CollectorConfig::edit_locked(&second_dir, |config| {
                config.lanes.etw.enabled = true;
                Ok(())
            })
            .unwrap();
            second_done_tx.send(()).unwrap();
        });
        assert!(
            second_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "the second edit must wait for the first process lock"
        );
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();

        let config = CollectorConfig::load(temp.path()).unwrap();
        assert_eq!(config.upload.destination, r"\\fileserver\share\eidos");
        assert!(config.lanes.etw.enabled);
    }
}
