use eidos_observe::{Capabilities, FeedCursor, SpoolStats};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Status,
    SessionKey { bytes: Vec<u8> },
    Mark { label: String },
    Export,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Status { status: CollectorStatus },
    Accepted,
    Exported { staged_file: String },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorStatus {
    pub schema: String,
    pub running: bool,
    pub uptime_s: u64,
    pub key_loaded: bool,
    pub capabilities: Capabilities,
    pub feed_cursor: Option<FeedCursor>,
    pub spool: SerializableSpoolStats,
    pub endpoint_events: EndpointEventCounts,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct EndpointEventCounts {
    pub opens: u64,
    pub closes: u64,
    pub mappings: u64,
    pub executions: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SerializableSpoolStats {
    pub records: u64,
    pub detailed_records: u64,
    pub detailed_bytes: u64,
    pub oldest_utc_ns: Option<i64>,
    pub newest_utc_ns: Option<i64>,
}

impl From<SpoolStats> for SerializableSpoolStats {
    fn from(value: SpoolStats) -> Self {
        Self {
            records: value.records,
            detailed_records: value.detailed_records,
            detailed_bytes: value.detailed_bytes,
            oldest_utc_ns: value.oldest_utc_ns,
            newest_utc_ns: value.newest_utc_ns,
        }
    }
}
