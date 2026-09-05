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
use eidos_domain::UnixNanos;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use ts_rs::TS;

pub const CONFIG_FILE: &str = "config.json";
const CONFIG_LOCK_FILE: &str = "config.lock";
static CONFIG_EDIT_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

/// Default port of the dedicated sync endpoint.
pub const DEFAULT_SYNC_PORT: u16 = 7710;
pub const MAX_BATCH_ROWS: u32 = 10_000;

/// A node's durable, certificate-pinned request to join a master. The
/// service retries pending attempts after restarts; a rejection remains
/// visible until the operator cancels it or starts a new attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PendingJoinTarget {
    pub request_id: String,
    pub endpoint: String,
    pub master_name: String,
    pub master_fingerprint: String,
    pub requested_at: UnixNanos,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub rejected_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
pub struct FleetConfig {
    /// Approve join requests and replicate joined nodes' sources here.
    pub central: bool,
    /// Address of the dedicated sync listener; `null` accepts no inbound
    /// sessions (a node that only dials out).
    pub listen: Option<String>,
    /// Join attempt waiting for the named master's approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pending_join: Option<PendingJoinTarget>,
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
            pending_join: None,
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
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("entropy unavailable: {e}"))?;
        let tmp = path.with_extension(format!(
            "json.{}.{:016x}.tmp",
            std::process::id(),
            u64::from_le_bytes(nonce)
        ));
        let result = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            drop(file);
            replace_config_file(&tmp, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Serialize a read-modify-write across service requests and any other
    /// process using this data directory.
    pub fn edit_locked(
        data_dir: &Path,
        edit: impl FnOnce(&mut Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        use fs4::fs_std::FileExt;
        let _process_guard = CONFIG_EDIT_LOCK.lock();
        let path = Self::path(data_dir);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("fleet config has no parent directory"))?;
        std::fs::create_dir_all(parent)?;
        let lock_path = parent.join(CONFIG_LOCK_FILE);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.lock_exclusive()?;
        let mut config = Self::load(data_dir)?;
        edit(&mut config)?;
        config.store(data_dir)?;
        Ok(config)
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

#[cfg(not(windows))]
fn replace_config_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::rename(from, to)?;
    Ok(())
}

#[cfg(windows)]
fn replace_config_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    if !to.exists() {
        std::fs::rename(from, to)?;
        return Ok(());
    }
    let mut from_wide: Vec<u16> = from.as_os_str().encode_wide().collect();
    from_wide.push(0);
    let mut to_wide: Vec<u16> = to.as_os_str().encode_wide().collect();
    to_wide.push(0);
    // SAFETY: both paths are owned, NUL-terminated UTF-16 buffers that live
    // through the call; backup and reserved pointers are intentionally null.
    let replaced = unsafe {
        ReplaceFileW(
            to_wide.as_ptr(),
            from_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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

    #[test]
    fn locked_concurrent_edits_preserve_both_changes() {
        let dir = tempfile::tempdir().unwrap();
        FleetConfig::default().store(dir.path()).unwrap();
        let path = std::sync::Arc::new(dir.path().to_path_buf());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_path = path.clone();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            FleetConfig::edit_locked(&first_path, |config| {
                config.central = true;
                Ok(())
            })
            .unwrap();
        });
        let second_path = path.clone();
        let second = std::thread::spawn(move || {
            barrier.wait();
            FleetConfig::edit_locked(&second_path, |config| {
                config.batch_rows = 777;
                Ok(())
            })
            .unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
        let config = FleetConfig::load(&path).unwrap();
        assert!(config.central);
        assert_eq!(config.batch_rows, 777);
    }
}
