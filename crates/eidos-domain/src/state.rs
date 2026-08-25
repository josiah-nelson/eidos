//! Source, object, content-processing, policy, and job state vocabularies.
//!
//! These enums are stored in the catalog (as their `as_str` forms), exposed by
//! the API, and rendered by the UI. Adding a variant is a compatible change;
//! renaming or removing one requires a migration.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $( $(#[$vmeta:meta])* $variant:ident => $s:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $( $(#[$vmeta])* $variant ),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$( $name::$variant ),+];

            pub const fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $s ),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $( $s => Some($name::$variant), )+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = UnknownVariant;
            fn from_str(s: &str) -> Result<Self, UnknownVariant> {
                Self::parse(s).ok_or_else(|| UnknownVariant { ty: stringify!($name), value: s.to_string() })
            }
        }
    };
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown {ty} value: {value:?}")]
pub struct UnknownVariant {
    pub ty: &'static str,
    pub value: String,
}

str_enum! {
    /// Lifecycle state of a configured source. See SPEC 7.1.
    pub enum SourceState {
        /// Configured but never scanned.
        New => "new",
        /// Initial or full enumeration in progress; no complete generation yet.
        Enumerating => "enumerating",
        /// A metadata generation is published; content processing not started.
        MetadataComplete => "metadata_complete",
        /// Metadata complete; content jobs draining.
        ContentPending => "content_pending",
        /// Metadata and content are complete for the published generation.
        Complete => "complete",
        /// Change feed overflowed or checkpoint invalid; results preserved as stale until reconciled.
        Degraded => "degraded",
        /// Source unreachable; last known results preserved.
        Offline => "offline",
        /// Freshness guarantee expired (e.g. generic SMB past its reconciliation interval).
        Stale => "stale",
        /// Reconciliation scan comparing observed and catalog state.
        Reconciling => "reconciling",
        /// Explicitly retired by the user; results hidden by default.
        Retired => "retired",
    }
}

impl SourceState {
    /// Whether search results from this source can be presented as complete
    /// for metadata queries.
    pub fn metadata_complete(self) -> bool {
        matches!(
            self,
            Self::MetadataComplete | Self::ContentPending | Self::Complete | Self::Reconciling
        )
    }
}

str_enum! {
    /// Kind of source adapter.
    pub enum SourceKind {
        /// Local Windows volume root with native NTFS/ReFS support.
        WindowsLocal => "windows_local",
        /// Windows path crawled generically (non-NTFS or subdirectory root).
        WindowsGeneric => "windows_generic",
        /// Local macOS volume with an FSEvents change feed.
        MacosLocal => "macos_local",
        /// macOS path crawled generically (no usable feed or subdirectory root).
        MacosGeneric => "macos_generic",
        /// Remote SMB share crawled generically with weak freshness semantics.
        Smb => "smb",
    }
}

str_enum! {
    /// Kind of filesystem object.
    pub enum ObjectKind {
        File => "file",
        Directory => "directory",
        /// Symlink, junction, or other reparse point. Target metadata is stored
        /// on the entry; traversal is policy-controlled.
        Reparse => "reparse",
        /// Virtual member inside an archive container.
        VirtualFile => "virtual_file",
        VirtualDirectory => "virtual_directory",
    }
}

impl ObjectKind {
    pub fn is_directory_like(self) -> bool {
        matches!(self, Self::Directory | Self::VirtualDirectory)
    }
    pub fn is_virtual(self) -> bool {
        matches!(self, Self::VirtualFile | Self::VirtualDirectory)
    }
}

str_enum! {
    /// Content-processing state of an object generation. See SPEC 7.6/7.8.
    pub enum ContentState {
        /// Not yet scheduled or still queued.
        Pending => "pending",
        /// Content fully indexed.
        Indexed => "indexed",
        /// Only part of the content is indexed (prefix/tail/sample); see coverage.
        Partial => "partial",
        /// Processing failed deterministically or exhausted retries.
        Failed => "failed",
        /// Content policy excluded the object (reason recorded).
        Excluded => "excluded",
        /// Sniffed as binary or otherwise unsupported for literal text.
        Unsupported => "unsupported",
        /// A newer object generation exists; indexed content refers to an older one.
        Stale => "stale",
        /// Not applicable (directories, reparse points).
        NotApplicable => "not_applicable",
    }
}

str_enum! {
    /// How much of the object's bytes the stored chunks cover.
    pub enum Coverage {
        Full => "full",
        Prefix => "prefix",
        Tail => "tail",
        Sample => "sample",
        None => "none",
    }
}

str_enum! {
    /// The three independent policy stages (SPEC 7.5).
    pub enum PolicyStage {
        Inventory => "inventory",
        Content => "content",
        Enrichment => "enrichment",
    }
}

str_enum! {
    /// Stable reason codes for policy decisions.
    pub enum ReasonCode {
        /// Included by default or by explicit include rule.
        Included => "included",
        /// User override forcing inclusion.
        UserInclude => "user_include",
        /// User override forcing exclusion.
        UserExclude => "user_exclude",
        VmDiskImage => "vm_disk_image",
        SwapOrHibernation => "swap_or_hibernation",
        RecycleBin => "recycle_bin",
        SystemVolumeInformation => "system_volume_information",
        KnownCache => "known_cache",
        DependencyCache => "dependency_cache",
        PackageArtifact => "package_artifact",
        BinaryData => "binary_data",
        MediaFile => "media_file",
        ArchiveContainer => "archive_container",
        PathRule => "path_rule",
        ExtensionRule => "extension_rule",
        ReparseNotTraversed => "reparse_not_traversed",
        /// File symlink / app-execution alias: the target is catalogued on its own.
        Symlink => "symlink",
        /// Cloud, projected-filesystem, or tiering placeholder: reading would hydrate it.
        Placeholder => "placeholder",
        /// Socket, FIFO, or device node surfaced as a file.
        SpecialFile => "special_file",
    }
}

str_enum! {
    /// Processing stage a job belongs to.
    pub enum JobStage {
        /// Apply catalog-critical change (delete, rename) to derived indexes.
        CatalogChange => "catalog_change",
        /// Publish metadata document to the catalog search index.
        MetadataProjection => "metadata_projection",
        /// Literal text extraction + content index.
        ContentText => "content_text",
        /// ZIP central-directory manifest.
        ArchiveManifest => "archive_manifest",
        /// Directory aggregate recomputation for a subtree.
        AggregateRebuild => "aggregate_rebuild",
        /// Reconciliation of a scope.
        Reconcile => "reconcile",
    }
}

str_enum! {
    pub enum JobState {
        Queued => "queued",
        Running => "running",
        Done => "done",
        Failed => "failed",
        /// Superseded by a newer generation of the same object.
        Superseded => "superseded",
    }
}

/// Priority classes from ARCHITECTURE.md section 8. Lower is more urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    CatalogCritical = 1,
    MetadataProjection = 2,
    SmallText = 3,
    NormalText = 4,
    LargeText = 5,
    ArchiveManifest = 6,
    Enrichment = 7,
}

impl Priority {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::CatalogCritical,
            2 => Self::MetadataProjection,
            3 => Self::SmallText,
            4 => Self::NormalText,
            5 => Self::LargeText,
            6 => Self::ArchiveManifest,
            7 => Self::Enrichment,
            _ => return None,
        })
    }
}

str_enum! {
    /// Why a job failed. Determines retry behaviour.
    pub enum FailureClass {
        /// Source temporarily unavailable, sharing violation, network error.
        Transient => "transient",
        /// Content type not supported by any extractor.
        Unsupported => "unsupported",
        /// Parser failed deterministically (corrupt input, invalid encoding).
        Deterministic => "deterministic",
        /// A resource limit (bytes, time, memory) was exceeded.
        ResourceLimit => "resource_limit",
        /// Input detected as corrupt.
        Corrupt => "corrupt",
    }
}

impl FailureClass {
    pub fn retryable(self) -> bool {
        matches!(self, Self::Transient)
    }
}

/// Windows file attribute bits preserved in the catalog (subset of
/// `FILE_ATTRIBUTE_*`). Stored as a raw `u32`; helpers decode common flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default, TS)]
#[serde(transparent)]
#[ts(as = "u32")]
pub struct FileAttributes(pub u32);

impl FileAttributes {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const TEMPORARY: u32 = 0x100;
    pub const SPARSE: u32 = 0x200;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const COMPRESSED: u32 = 0x800;
    pub const OFFLINE: u32 = 0x1000;
    pub const ENCRYPTED: u32 = 0x4000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

    pub const fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
    pub const fn is_directory(self) -> bool {
        self.has(Self::DIRECTORY)
    }
    pub const fn is_reparse(self) -> bool {
        self.has(Self::REPARSE_POINT)
    }
    pub const fn is_hidden(self) -> bool {
        self.has(Self::HIDDEN)
    }
    pub const fn is_system(self) -> bool {
        self.has(Self::SYSTEM)
    }

    /// Short textual flags, e.g. `RHS` (readonly, hidden, system).
    pub fn flags_string(self) -> String {
        let mut s = String::new();
        let table = [
            (Self::READONLY, 'R'),
            (Self::HIDDEN, 'H'),
            (Self::SYSTEM, 'S'),
            (Self::DIRECTORY, 'D'),
            (Self::ARCHIVE, 'A'),
            (Self::TEMPORARY, 'T'),
            (Self::SPARSE, 'P'),
            (Self::REPARSE_POINT, 'L'),
            (Self::COMPRESSED, 'C'),
            (Self::OFFLINE, 'O'),
            (Self::ENCRYPTED, 'E'),
        ];
        for (bit, ch) in table {
            if self.has(bit) {
                s.push(ch);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_roundtrip_all_enums() {
        for s in SourceState::ALL {
            assert_eq!(SourceState::parse(s.as_str()), Some(*s));
        }
        for s in ContentState::ALL {
            assert_eq!(ContentState::parse(s.as_str()), Some(*s));
        }
        for s in ReasonCode::ALL {
            assert_eq!(ReasonCode::parse(s.as_str()), Some(*s));
        }
        for s in JobStage::ALL {
            assert_eq!(JobStage::parse(s.as_str()), Some(*s));
        }
        assert!(SourceState::parse("bogus").is_none());
    }

    #[test]
    fn serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&SourceState::MetadataComplete).unwrap(),
            "\"metadata_complete\""
        );
        let back: ContentState = serde_json::from_str("\"not_applicable\"").unwrap();
        assert_eq!(back, ContentState::NotApplicable);
    }

    #[test]
    fn priority_ordering() {
        assert!(Priority::CatalogCritical < Priority::SmallText);
        assert_eq!(Priority::from_u8(5), Some(Priority::LargeText));
    }

    #[test]
    fn attributes() {
        let a = FileAttributes(
            FileAttributes::HIDDEN | FileAttributes::SYSTEM | FileAttributes::DIRECTORY,
        );
        assert!(a.is_directory());
        assert_eq!(a.flags_string(), "HSD");
    }
}
