//! Search result and completeness contracts.
//!
//! Every search response carries source completeness (ARCHITECTURE invariant
//! 4). The UI never infers completeness from the absence of results.

use crate::ids::{EntryId, HostId, ObjectId, SourceId};
use crate::query::FacetField;
use crate::state::{ContentState, Coverage, FileAttributes, ObjectKind, SourceState};
use crate::time::UnixNanos;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub schema_version: u32,
    pub hits: Vec<Hit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub total: TotalCount,
    pub timing: Timing,
    /// One entry per source in scope, always present.
    pub completeness: Vec<SourceCompleteness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<Explanation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<Facet>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SearchResponse {
    /// True only when every in-scope source reports the completeness level the
    /// query requires. Used by the CLI exit status and UI banners.
    pub fn all_sources_complete(&self, needs_content: bool) -> bool {
        self.completeness
            .iter()
            .all(|c| c.metadata_complete && (!needs_content || c.content_complete))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TotalCount {
    pub value: u64,
    /// `false` when `value` is a lower bound.
    pub exact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Timing {
    pub total_ms: f64,
    pub plan_ms: f64,
    pub retrieve_ms: f64,
    pub verify_ms: f64,
    pub join_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub object_id: ObjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<EntryId>,
    pub source_id: SourceId,
    pub host_id: HostId,
    pub kind: ObjectKind,
    pub name: String,
    /// Current rendered path. May be `None` for orphaned objects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<ObjectId>,
    /// Lowercase extension without dot; empty for none.
    pub extension: String,
    pub size: u64,
    pub allocated_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<UnixNanos>,
    pub attributes: FileAttributes,
    pub hard_link_count: u32,
    pub content: ContentSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<Snippet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<DirectorySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveSummary>,
    /// Source state at response time, duplicated for row-level rendering.
    pub source_state: SourceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSummary {
    pub state: ContentState,
    pub coverage: Coverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ContentSummary {
    pub fn not_applicable() -> Self {
        Self {
            state: ContentState::NotApplicable,
            coverage: Coverage::None,
            indexed_bytes: None,
            content_id: None,
            reason: None,
        }
    }
    pub fn pending() -> Self {
        Self {
            state: ContentState::Pending,
            coverage: Coverage::None,
            indexed_bytes: None,
            content_id: None,
            reason: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snippet {
    pub chunk_ordinal: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub text: String,
    /// Character (not byte) ranges within `text` to highlight.
    #[serde(default)]
    pub highlights: Vec<[u32; 2]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectorySummary {
    pub file_count: u64,
    pub directory_count: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    /// Kept separate from physical apparent bytes.
    pub archive_declared_bytes: u64,
    /// Kept separate from physical allocated bytes.
    pub archive_compressed_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_modified: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_modified: Option<UnixNanos>,
    /// Sparse descendant extension counts (top entries only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extension_counts: BTreeMap<String, u64>,
    /// Whether the aggregate reflects a published, complete generation.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveSummary {
    pub container_id: ObjectId,
    pub depth: u32,
    pub member_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceCompleteness {
    pub source_id: SourceId,
    pub name: String,
    pub state: SourceState,
    /// The published metadata generation is complete for this source.
    pub metadata_complete: bool,
    /// No content jobs are outstanding for the published generation.
    pub content_complete: bool,
    pub content_pending: u64,
    pub content_failed: u64,
    /// Directories that could not be listed in the published generation.
    /// Their previous contents (if any) are preserved; aggregates beneath
    /// them are flagged incomplete.
    #[serde(default)]
    pub listing_errors: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan_completed: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_age_ms: Option<u64>,
    pub freshness: Freshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Strength of the freshness guarantee for a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Native change feed (USN) active and checkpoint valid.
    Live,
    /// Periodic reconciliation only (generic/SMB sources).
    Periodic,
    /// Change feed degraded or unknown.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    pub readable: String,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub stage: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Facet {
    pub field: FacetField,
    pub values: Vec<FacetValue>,
    /// True when more values existed than were returned.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetValue {
    pub value: String,
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completeness_logic() {
        let mk = |m, c| SourceCompleteness {
            source_id: SourceId(1),
            name: "x".into(),
            state: SourceState::ContentPending,
            metadata_complete: m,
            content_complete: c,
            content_pending: 0,
            content_failed: 0,
            listing_errors: 0,
            last_scan_completed: None,
            checkpoint_age_ms: None,
            freshness: Freshness::Live,
            note: None,
        };
        let resp = SearchResponse {
            schema_version: 1,
            hits: vec![],
            next_cursor: None,
            total: TotalCount {
                value: 0,
                exact: true,
            },
            timing: Timing::default(),
            completeness: vec![mk(true, false)],
            explanation: None,
            facets: vec![],
            warnings: vec![],
        };
        assert!(resp.all_sources_complete(false));
        assert!(!resp.all_sources_complete(true));
    }
}
