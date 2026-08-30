//! Catalog row types.

use eidos_domain::{
    ContentId, ContentState, FileAttributes, HostId, IdentityConfidence, NativeIdentity, ObjectId,
    ObjectKind, SourceId, SourceKind, SourceState, SyncPolicy, UnixNanos, VolumeId,
};
use rusqlite::Row;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct NewSource {
    pub host_id: HostId,
    pub name: String,
    pub kind: SourceKind,
    pub root_path: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SourceRecord {
    pub id: SourceId,
    pub host_id: HostId,
    pub name: String,
    pub kind: SourceKind,
    pub root_path: String,
    pub aliases: Vec<String>,
    pub state: SourceState,
    pub state_reason: Option<String>,
    pub policy_version: u32,
    pub root_object_id: Option<ObjectId>,
    pub published_generation: Option<i64>,
    pub volume_id: Option<VolumeId>,
    pub preserve_offline: bool,
    /// Seconds between periodic reconciliations for feed-less sources.
    pub reconcile_interval_s: Option<i64>,
    /// Literal-text extraction enabled for this source (default on).
    pub content_enabled: bool,
    /// Concurrent content jobs allowed on this source (HDD-aware budget).
    pub content_concurrency: u32,
    /// Fleet replication policy: `inherit` follows enrollment, `local_only`
    /// never ships (a `remote` source is never a shipper).
    pub sync_policy: SyncPolicy,
    pub checkpoint_kind: Option<String>,
    pub checkpoint_at: Option<UnixNanos>,
    pub last_scan_started_at: Option<UnixNanos>,
    pub last_scan_completed_at: Option<UnixNanos>,
    pub created_at: UnixNanos,
    pub updated_at: UnixNanos,
}

impl SourceRecord {
    pub(crate) fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        let aliases: String = r.get("aliases")?;
        Ok(Self {
            id: SourceId(r.get("source_id")?),
            host_id: HostId(r.get("host_id")?),
            name: r.get("name")?,
            kind: parse_enum::<SourceKind>(r.get::<_, String>("kind")?),
            root_path: r.get("root_path")?,
            aliases: serde_json::from_str(&aliases).unwrap_or_default(),
            state: parse_enum::<SourceState>(r.get::<_, String>("state")?),
            state_reason: r.get("state_reason")?,
            policy_version: r.get::<_, i64>("policy_version")? as u32,
            root_object_id: r.get::<_, Option<i64>>("root_object_id")?.map(ObjectId),
            published_generation: r.get("published_generation")?,
            volume_id: r.get::<_, Option<i64>>("volume_id")?.map(VolumeId),
            preserve_offline: r.get::<_, i64>("preserve_offline")? != 0,
            reconcile_interval_s: r.get("reconcile_interval_s")?,
            content_enabled: r.get::<_, i64>("content_enabled")? != 0,
            content_concurrency: r.get::<_, i64>("content_concurrency")?.max(1) as u32,
            sync_policy: parse_enum::<SyncPolicy>(r.get::<_, String>("sync_policy")?),
            checkpoint_kind: r.get("checkpoint_kind")?,
            checkpoint_at: r.get::<_, Option<i64>>("checkpoint_at")?.map(UnixNanos),
            last_scan_started_at: r
                .get::<_, Option<i64>>("last_scan_started_at")?
                .map(UnixNanos),
            last_scan_completed_at: r
                .get::<_, Option<i64>>("last_scan_completed_at")?
                .map(UnixNanos),
            created_at: UnixNanos(r.get("created_at")?),
            updated_at: UnixNanos(r.get("updated_at")?),
        })
    }
}

pub(crate) trait EnumFallback: std::str::FromStr {
    const FALLBACK: Self;
}
impl EnumFallback for SourceKind {
    const FALLBACK: Self = SourceKind::WindowsGeneric;
}
impl EnumFallback for SourceState {
    const FALLBACK: Self = SourceState::New;
}
impl EnumFallback for ObjectKind {
    const FALLBACK: Self = ObjectKind::File;
}
impl EnumFallback for ContentState {
    const FALLBACK: Self = ContentState::Pending;
}
impl EnumFallback for SyncPolicy {
    const FALLBACK: Self = SyncPolicy::Inherit;
}

pub(crate) fn parse_enum<T: EnumFallback>(s: String) -> T {
    s.parse().unwrap_or(T::FALLBACK)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ObjectRecord {
    pub id: ObjectId,
    pub source_id: SourceId,
    pub kind: ObjectKind,
    pub native: Option<NativeIdentity>,
    pub identity_confidence: IdentityConfidence,
    pub generation: u32,
    pub size: u64,
    pub allocated: u64,
    pub attributes: FileAttributes,
    pub created: Option<UnixNanos>,
    pub modified: Option<UnixNanos>,
    pub changed: Option<UnixNanos>,
    pub accessed: Option<UnixNanos>,
    pub reparse_tag: u32,
    pub link_count: u32,
    pub content_state: ContentState,
    pub content_id: Option<ContentId>,
    pub first_seen_generation: i64,
    pub last_seen_generation: i64,
    pub deleted_at: Option<UnixNanos>,
}

pub(crate) const OBJECT_COLUMNS: &str = "o.object_id, o.source_id, o.kind, o.native_volume_serial, o.native_id_high, o.native_id_low, \
     o.identity_confidence, o.generation, o.size, o.allocated, o.attributes, o.created, o.modified, \
     o.changed, o.accessed, o.reparse_tag, o.link_count, o.content_state, o.content_id, \
     o.first_seen_generation, o.last_seen_generation, o.deleted_at";

impl ObjectRecord {
    /// Read from a row whose first 22 columns are `OBJECT_COLUMNS` in order,
    /// starting at `base`.
    pub(crate) fn from_row_at(r: &Row<'_>, base: usize) -> rusqlite::Result<Self> {
        let confidence_s: String = r.get(base + 6)?;
        let confidence = IdentityConfidence::from_str_opt(&confidence_s)
            .unwrap_or(IdentityConfidence::PathDerived);
        let serial: Option<i64> = r.get(base + 3)?;
        let high: Option<i64> = r.get(base + 4)?;
        let low: Option<i64> = r.get(base + 5)?;
        let native = match (serial, high, low) {
            (Some(s), Some(h), Some(l)) => Some(NativeIdentity {
                volume_serial: s as u64,
                file_id_high: h as u64,
                file_id_low: l as u64,
                confidence,
            }),
            _ => None,
        };
        let content_id: Option<Vec<u8>> = r.get(base + 18)?;
        Ok(Self {
            id: ObjectId(r.get(base)?),
            source_id: SourceId(r.get(base + 1)?),
            kind: parse_enum(r.get::<_, String>(base + 2)?),
            native,
            identity_confidence: confidence,
            generation: r.get::<_, i64>(base + 7)? as u32,
            size: r.get::<_, i64>(base + 8)? as u64,
            allocated: r.get::<_, i64>(base + 9)? as u64,
            attributes: FileAttributes(r.get::<_, i64>(base + 10)? as u32),
            created: r.get::<_, Option<i64>>(base + 11)?.map(UnixNanos),
            modified: r.get::<_, Option<i64>>(base + 12)?.map(UnixNanos),
            changed: r.get::<_, Option<i64>>(base + 13)?.map(UnixNanos),
            accessed: r.get::<_, Option<i64>>(base + 14)?.map(UnixNanos),
            reparse_tag: r.get::<_, i64>(base + 15)? as u32,
            link_count: r.get::<_, i64>(base + 16)? as u32,
            content_state: parse_enum(r.get::<_, String>(base + 17)?),
            content_id: content_id.and_then(|v| {
                if v.len() == 32 {
                    let mut a = [0u8; 32];
                    a.copy_from_slice(&v);
                    Some(ContentId(a))
                } else {
                    None
                }
            }),
            first_seen_generation: r.get(base + 19)?,
            last_seen_generation: r.get(base + 20)?,
            deleted_at: r.get::<_, Option<i64>>(base + 21)?.map(UnixNanos),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct EntryRecord {
    pub id: eidos_domain::EntryId,
    pub source_id: SourceId,
    pub parent_id: Option<ObjectId>,
    pub object_id: ObjectId,
    pub name: String,
    pub extension: String,
    pub is_virtual: bool,
    pub first_seen_generation: i64,
    pub last_seen_generation: i64,
    pub deleted_at: Option<UnixNanos>,
}

pub(crate) const ENTRY_COLUMNS: &str =
    "e.entry_id, e.source_id, e.parent_id, e.object_id, e.name, e.extension, e.is_virtual, \
     e.first_seen_generation, e.last_seen_generation, e.deleted_at";

impl EntryRecord {
    pub(crate) fn from_row_at(r: &Row<'_>, base: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            id: eidos_domain::EntryId(r.get(base)?),
            source_id: SourceId(r.get(base + 1)?),
            parent_id: r.get::<_, Option<i64>>(base + 2)?.map(ObjectId),
            object_id: ObjectId(r.get(base + 3)?),
            name: r.get(base + 4)?,
            extension: r.get(base + 5)?,
            is_virtual: r.get::<_, i64>(base + 6)? != 0,
            first_seen_generation: r.get(base + 7)?,
            last_seen_generation: r.get(base + 8)?,
            deleted_at: r.get::<_, Option<i64>>(base + 9)?.map(UnixNanos),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DirectoryAggregate {
    pub object_id: ObjectId,
    pub file_count: u64,
    pub dir_count: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub newest_modified: Option<UnixNanos>,
    pub oldest_modified: Option<UnixNanos>,
    pub content_pending: u64,
    pub content_indexed: u64,
    pub content_failed: u64,
    pub content_excluded: u64,
    pub generation: i64,
    pub complete: bool,
}

pub(crate) const AGG_COLUMNS: &str = "a.object_id, a.file_count, a.dir_count, a.logical_bytes, a.allocated_bytes, a.newest_modified, \
     a.oldest_modified, a.content_pending, a.content_indexed, a.content_failed, a.content_excluded, \
     a.generation, a.complete";

impl DirectoryAggregate {
    pub(crate) fn from_row_at(r: &Row<'_>, base: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            object_id: ObjectId(r.get(base)?),
            file_count: r.get::<_, i64>(base + 1)? as u64,
            dir_count: r.get::<_, i64>(base + 2)? as u64,
            logical_bytes: r.get::<_, i64>(base + 3)? as u64,
            allocated_bytes: r.get::<_, i64>(base + 4)? as u64,
            newest_modified: r.get::<_, Option<i64>>(base + 5)?.map(UnixNanos),
            oldest_modified: r.get::<_, Option<i64>>(base + 6)?.map(UnixNanos),
            content_pending: r.get::<_, i64>(base + 7)? as u64,
            content_indexed: r.get::<_, i64>(base + 8)? as u64,
            content_failed: r.get::<_, i64>(base + 9)? as u64,
            content_excluded: r.get::<_, i64>(base + 10)? as u64,
            generation: r.get(base + 11)?,
            complete: r.get::<_, i64>(base + 12)? != 0,
        })
    }

    /// Optional variant for LEFT JOINs: returns `None` if `object_id` is NULL.
    pub(crate) fn from_row_opt(r: &Row<'_>, base: usize) -> rusqlite::Result<Option<Self>> {
        let id: Option<i64> = r.get(base)?;
        if id.is_none() {
            return Ok(None);
        }
        Self::from_row_at(r, base).map(Some)
    }
}

/// One child of a directory as returned by browse APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ChildRow {
    pub entry: EntryRecord,
    pub object: ObjectRecord,
    pub aggregate: Option<DirectoryAggregate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, TS)]
#[serde(rename_all = "snake_case")]
pub enum ChildSort {
    #[default]
    Name,
    Size,
    AllocatedSize,
    Modified,
    Kind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ChildrenPage {
    pub sort: ChildSort,
    pub descending: bool,
    pub offset: u64,
    pub limit: u32,
    /// Omit hidden/system entries (e.g. `$RECYCLE.BIN`) by default.
    pub include_hidden: bool,
}

impl Default for ChildrenPage {
    fn default() -> Self {
        Self {
            sort: ChildSort::Name,
            descending: false,
            offset: 0,
            limit: 200,
            include_hidden: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ChildrenResult {
    pub rows: Vec<ChildRow>,
    pub total: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, TS)]
pub struct SourceCounts {
    pub objects: u64,
    pub entries: u64,
    pub directories: u64,
    pub files: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub content_pending: u64,
    pub content_indexed: u64,
    pub content_failed: u64,
    pub content_excluded: u64,
    pub content_unsupported: u64,
    pub open_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(rename = "ScanGeneration")]
pub struct ScanGenerationRecord {
    pub source_id: SourceId,
    pub generation: i64,
    pub kind: String,
    pub state: String,
    pub started_at: UnixNanos,
    pub finished_at: Option<UnixNanos>,
    pub published_at: Option<UnixNanos>,
    pub dirs_listed: u64,
    pub entries_seen: u64,
    pub errors: u64,
    pub tombstoned: u64,
    pub note: Option<String>,
}

impl ScanGenerationRecord {
    pub(crate) fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            source_id: SourceId(r.get("source_id")?),
            generation: r.get("generation")?,
            kind: r.get("kind")?,
            state: r.get("state")?,
            started_at: UnixNanos(r.get("started_at")?),
            finished_at: r.get::<_, Option<i64>>("finished_at")?.map(UnixNanos),
            published_at: r.get::<_, Option<i64>>("published_at")?.map(UnixNanos),
            dirs_listed: r.get::<_, i64>("dirs_listed")? as u64,
            entries_seen: r.get::<_, i64>("entries_seen")? as u64,
            errors: r.get::<_, i64>("errors")? as u64,
            tombstoned: r.get::<_, i64>("tombstoned")? as u64,
            note: r.get("note")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ErrorRecord {
    pub id: i64,
    pub source_id: SourceId,
    pub object_id: Option<ObjectId>,
    pub generation: Option<i64>,
    pub stage: String,
    pub kind: String,
    pub code: i64,
    pub path: String,
    pub message: String,
    pub occurred_at: UnixNanos,
    pub resolved_at: Option<UnixNanos>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(rename = "PolicyDecision")]
pub struct PolicyDecisionRecord {
    pub object_id: ObjectId,
    pub stage: String,
    pub included: bool,
    pub reason: String,
    pub rule: String,
    pub policy_version: u32,
    pub user_override: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ExtensionCount {
    pub extension: String,
    pub count: u64,
    pub bytes: u64,
}
