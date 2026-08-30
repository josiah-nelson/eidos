//! Fleet configuration file: `fleet/config.json` under the data directory.
//!
//! The roster (which peers are trusted, where the central is) lives in the
//! catalog; this file holds what the running service needs to know about
//! itself: whether it accepts the central role, where it listens, and the
//! bounds it enforces. The service re-reads it on every scheduler tick, so
//! `eidos fleet ...` commands take effect without a restart.

use crate::wire::{
    DEFAULT_BATCH_BYTES, DEFAULT_BATCH_ROWS, DEFAULT_CREDIT_BYTES, DEFAULT_MAX_FRAME_BYTES,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use ts_rs::TS;

pub const CONFIG_FILE: &str = "config.json";

/// Default port of the dedicated sync endpoint.
pub const DEFAULT_SYNC_PORT: u16 = 7710;
pub const MAX_BATCH_ROWS: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
pub struct FleetConfig {
    /// Accept enrollments and replicate enrolled nodes' sources here.
    pub central: bool,
    /// Address of the dedicated sync listener; `null` accepts no inbound
    /// sessions (a node that only dials out).
    pub listen: Option<String>,
    /// Largest frame accepted or sent.
    pub max_frame_bytes: u64,
    /// Bytes a peer may have in flight towards this side.
    pub credit_bytes: u64,
    pub batch_rows: u32,
    pub batch_bytes: u64,
    /// Seconds between reconnect attempts, at most.
    pub reconnect_max_secs: u64,
    /// Unacknowledged ledger rows per source above which the node reports
    /// a degraded backlog (never dropped).
    pub backlog_ceiling_rows: u64,
    /// Unacknowledged tombstones per source above which the node reports a
    /// degraded backlog.
    pub backlog_ceiling_tombstones: u64,
    /// Merkle leaf bits used for repair offers; `null` picks by source size.
    /// The transport clamps this to 10..=17 so the complete compact manifest
    /// fits inside the protocol's 16 MiB frame ceiling.
    pub repair_leaf_bits: Option<u8>,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            central: false,
            listen: None,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES as u64,
            credit_bytes: DEFAULT_CREDIT_BYTES,
            batch_rows: DEFAULT_BATCH_ROWS,
            batch_bytes: DEFAULT_BATCH_BYTES as u64,
            reconnect_max_secs: 60,
            backlog_ceiling_rows: 5_000_000,
            backlog_ceiling_tombstones: 1_000_000,
            repair_leaf_bits: None,
        }
    }
}

impl FleetConfig {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir
            .join(
                crate::identity::NodeIdentity::fleet_dir(data_dir)
                    .file_name()
                    .unwrap(),
            )
            .join(CONFIG_FILE)
    }

    /// Read the file, or the defaults when there is none. An unreadable
    /// file is an error rather than a silent fallback: a typo must not
    /// turn a central back into a standalone.
    pub fn load(data_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(data_dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    pub fn store(&self, data_dir: &Path) -> anyhow::Result<()> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn max_frame(&self) -> usize {
        usize::try_from(self.max_frame_bytes)
            .unwrap_or(usize::MAX)
            .clamp(64 * 1024, DEFAULT_MAX_FRAME_BYTES)
    }

    pub fn batch_bytes(&self) -> usize {
        let max = self.max_frame() / 2;
        let min = (64 * 1024).min(max);
        usize::try_from(self.batch_bytes)
            .unwrap_or(usize::MAX)
            .clamp(min, max)
    }

    pub fn credit_bytes(&self) -> u64 {
        self.credit_bytes.min(256 * 1024 * 1024)
    }

    pub fn batch_rows(&self) -> u32 {
        self.batch_rows.clamp(1, MAX_BATCH_ROWS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            FleetConfig::load(dir.path()).unwrap(),
            FleetConfig::default()
        );
        let cfg = FleetConfig {
            central: true,
            listen: Some("0.0.0.0:7710".into()),
            ..FleetConfig::default()
        };
        cfg.store(dir.path()).unwrap();
        assert_eq!(FleetConfig::load(dir.path()).unwrap(), cfg);
        std::fs::write(FleetConfig::path(dir.path()), b"{ nope").unwrap();
        assert!(FleetConfig::load(dir.path()).is_err());
    }

    #[test]
    fn minimum_frame_configuration_has_a_valid_batch_limit() {
        let cfg = FleetConfig {
            max_frame_bytes: 1,
            batch_bytes: u64::MAX,
            ..FleetConfig::default()
        };
        assert_eq!(cfg.max_frame(), 64 * 1024);
        assert_eq!(cfg.batch_bytes(), 32 * 1024);
        assert_eq!(cfg.credit_bytes(), DEFAULT_CREDIT_BYTES);

        let cfg = FleetConfig {
            credit_bytes: u64::MAX,
            ..FleetConfig::default()
        };
        assert_eq!(cfg.credit_bytes(), 256 * 1024 * 1024);

        let cfg = FleetConfig {
            max_frame_bytes: u64::MAX,
            batch_rows: u32::MAX,
            ..FleetConfig::default()
        };
        assert_eq!(cfg.max_frame(), DEFAULT_MAX_FRAME_BYTES);
        assert_eq!(cfg.batch_rows(), MAX_BATCH_ROWS);
    }
}
