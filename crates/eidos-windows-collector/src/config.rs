//! Collector configuration: a JSON file in the data directory, written by
//! `observe init`, adjusted at runtime through the control pipe, and hashed
//! into every bundle manifest so exports say which lanes produced them.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

    /// Stable hash of the collection-affecting configuration.
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_vec(self).unwrap_or_default();
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
}
