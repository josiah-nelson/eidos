//! Control-pipe protocol between `eidos observe` and the collector service.
//!
//! Frames are a little-endian `u32` length followed by JSON, bounded to
//! 64 KiB in each direction. Status output is local operator information
//! and may name drive letters; it is never part of an export.

use eidos_observe::{Capabilities, DropCounters, LaneStates};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Attach a keyed phase marker; the label itself is never persisted.
    Mark {
        label: String,
    },
    /// Stage a bundle in the export directory and return its path.
    Export,
    /// Toggle lanes at runtime; persisted into the configuration file.
    SetLanes {
        usn: Option<bool>,
        etw: Option<bool>,
        content: Option<bool>,
        enumeration: Option<bool>,
    },
    /// Run one read-only enumeration probe now (all fixed volumes when
    /// `volume` is `None`; otherwise a drive root such as `D:\`).
    Probe {
        volume: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Status { status: Box<CollectorStatus> },
    Accepted,
    Exported { staged_file: PathBuf },
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectorStatus {
    pub version: String,
    pub build_hash: String,
    pub config_hash: String,
    pub uptime_s: u64,
    pub capabilities: Capabilities,
    pub lanes: LaneStates,
    pub spool: SpoolView,
    pub drops: DropCounters,
    pub capture_gaps: usize,
    pub volumes: Vec<VolumeView>,
    pub feeds: Vec<FeedView>,
    pub etw: EtwView,
    pub collector: ProcessView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpoolView {
    pub records: u64,
    pub detailed_records: u64,
    pub detailed_bytes: u64,
    pub oldest_utc_ns: Option<i64>,
    pub newest_utc_ns: Option<i64>,
    pub file_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeView {
    pub root: String,
    pub filesystem: String,
    pub drive: String,
    pub bus: String,
    pub media: String,
    pub journaled: bool,
    pub excluded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedView {
    pub root: String,
    pub state: String,
    pub detail: Option<String>,
    pub batches: u64,
    pub records: u64,
    pub logical_changes: u64,
    pub lag_bytes: u64,
    pub last_batch_s_ago: Option<u64>,
    pub overflows: u64,
    pub recreations: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtwView {
    pub state: String,
    pub window_open: bool,
    pub next_window_s: Option<u64>,
    pub events: u64,
    /// Events the kernel reported losing before the consumer saw them.
    pub lost_events: u64,
    /// Events the consumer dropped because the aggregator was behind.
    pub queue_dropped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessView {
    pub cpu_ms: u64,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub handles: u32,
    pub threads: u32,
}

pub fn encode(value: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BYTES {
        anyhow::bail!("frame of {} bytes exceeds the 64 KiB bound", body.len());
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub fn read_frame(reader: &mut impl std::io::Read) -> anyhow::Result<Vec<u8>> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        anyhow::bail!("frame of {length} bytes exceeds the 64 KiB bound");
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_are_bounded() {
        let request = Request::Mark {
            label: "phase-a".into(),
        };
        let frame = encode(&request).unwrap();
        let body = read_frame(&mut frame.as_slice()).unwrap();
        assert_eq!(serde_json::from_slice::<Request>(&body).unwrap(), request);

        let huge = Request::Mark {
            label: "x".repeat(MAX_FRAME_BYTES),
        };
        assert!(encode(&huge).is_err());
        let mut bad = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec();
        bad.extend_from_slice(b"{}");
        assert!(read_frame(&mut bad.as_slice()).is_err());
    }
}
