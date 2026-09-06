//! Exclusion and classification policy (SPEC 7.5).
//!
//! Every object gets three independent outcomes: inventory, content, and
//! enrichment. In v0.5 the inventory policy includes everything (size
//! analytics must see VM disks and the recycle bin); the content policy
//! excludes data that is never useful as literal text; enrichment has no
//! rules yet. Decisions are context-sensitive: a directory named `bin` is
//! never excluded by name alone.

use eidos_domain::{extension_of, FileAttributes, FileKind, ReasonCode};

pub const POLICY_VERSION: u32 = 3;

/// Reparse tags (winnt.h) that matter for the content policy. Values are
/// ABI-stable.
pub mod reparse {
    pub const SYMLINK: u32 = 0xA000_000C;
    pub const WIM: u32 = 0x8000_0008;
    pub const DEDUP: u32 = 0x8000_0013;
    pub const APPEXECLINK: u32 = 0x8000_001B;
    pub const WCI: u32 = 0x8000_0018;
    pub const STORAGE_SYNC: u32 = 0x8000_001E;
    pub const ONEDRIVE: u32 = 0x8000_0021;
    pub const AF_UNIX: u32 = 0x8000_0023;
    pub const LX_FIFO: u32 = 0x8000_0024;
    pub const LX_CHR: u32 = 0x8000_0025;
    pub const LX_BLK: u32 = 0x8000_0026;
    pub const LX_SYMLINK: u32 = 0xA000_001D;
    pub const PROJFS: u32 = 0x9000_001C;
    pub const PROJFS_TNG: u32 = 0x9000_0018;
    /// `IO_REPARSE_TAG_CLOUD` family: `0x9000x01A` where `x` is a provider nibble.
    pub const CLOUD: u32 = 0x9000_001A;
    pub const CLOUD_MASK: u32 = 0x0000_F000;

    pub fn is_cloud(tag: u32) -> bool {
        (tag & !CLOUD_MASK) == CLOUD
    }

    /// Classify a *file* reparse tag for content processing.
    pub fn content_rule(tag: u32) -> ReparseClass {
        match tag {
            0 => ReparseClass::Plain,
            SYMLINK | LX_SYMLINK | APPEXECLINK => ReparseClass::Symlink,
            AF_UNIX | LX_FIFO | LX_CHR | LX_BLK => ReparseClass::Special,
            PROJFS | PROJFS_TNG | STORAGE_SYNC | ONEDRIVE => ReparseClass::Placeholder,
            t if is_cloud(t) => ReparseClass::Placeholder,
            // WIM/dedup/WCI-backed data reads transparently.
            WIM | DEDUP | WCI => ReparseClass::Plain,
            // Unknown tags: the data stream is usually real (custom filters);
            // treat as readable and let sniffing decide.
            _ => ReparseClass::Plain,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReparseClass {
        Plain,
        Symlink,
        Special,
        Placeholder,
    }
}

/// Context inherited down a directory chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyCtx {
    pub depth: u32,
    /// Source-relative path with `/` separators (names retain their case).
    pub relative: String,
    /// Folded name of this directory (`""` for the root).
    pub name_folded: String,
    /// Content exclusion inherited from this directory or an ancestor.
    pub inherited_content_exclusion: Option<(ReasonCode, &'static str)>,
}

impl PolicyCtx {
    pub fn root() -> Self {
        Self {
            depth: 0,
            relative: String::new(),
            name_folded: String::new(),
            inherited_content_exclusion: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentDecision {
    /// Literal-text candidate; subject to sniffing when processed.
    Candidate,
    /// No extractor in this version (documents, archives).
    Unsupported,
    /// Index into this engine's ordered operator rules.
    Operator { include: bool, index: usize },
    /// Excluded by policy with a stable reason and the rule that fired.
    Excluded {
        reason: ReasonCode,
        rule: &'static str,
    },
}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    pub version: u32,
    pub(crate) revision: u32,
    pub(crate) rules: Vec<crate::exclusions::CompiledRule>,
    pub(crate) protected: Vec<String>,
    pub(crate) absolute_protected: Vec<String>,
    pub(crate) case_sensitive: bool,
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyEngine {
    pub fn new() -> Self {
        Self {
            version: POLICY_VERSION,
            revision: 0,
            rules: Vec::new(),
            protected: Vec::new(),
            absolute_protected: Vec::new(),
            case_sensitive: false,
        }
    }

    /// Derive the context for a child directory.
    pub fn directory(&self, name: &str, parent: &PolicyCtx) -> PolicyCtx {
        let folded = fold(name);
        let mut inherited = parent.inherited_content_exclusion;
        if inherited.is_none() {
            inherited = directory_rule(&folded, parent);
        }
        PolicyCtx {
            depth: parent.depth + 1,
            relative: Self::child_path(parent, name),
            name_folded: folded,
            inherited_content_exclusion: inherited,
        }
    }

    /// Content decision for a file.
    pub fn file(
        &self,
        name: &str,
        attributes: FileAttributes,
        reparse_tag: u32,
        parent: &PolicyCtx,
    ) -> ContentDecision {
        // The source-relative path costs an allocation for every file in
        // every scan. Build it only when a rule or a protected boundary can
        // actually consult it; the default policy has neither.
        let relative = (!self.protected.is_empty() || !self.rules.is_empty())
            .then(|| Self::child_path(parent, name));
        if let Some(relative) = &relative {
            if self.is_protected(relative) {
                return ContentDecision::Excluded {
                    reason: ReasonCode::SelfStore,
                    rule: "self-store",
                };
            }
        }
        if attributes.is_reparse() || reparse_tag != 0 {
            match reparse::content_rule(reparse_tag) {
                reparse::ReparseClass::Symlink => {
                    return ContentDecision::Excluded {
                        reason: ReasonCode::Symlink,
                        rule: "reparse-symlink",
                    }
                }
                reparse::ReparseClass::Special => {
                    return ContentDecision::Excluded {
                        reason: ReasonCode::SpecialFile,
                        rule: "reparse-special-file",
                    }
                }
                reparse::ReparseClass::Placeholder => {
                    return ContentDecision::Excluded {
                        reason: ReasonCode::Placeholder,
                        rule: "reparse-placeholder",
                    }
                }
                reparse::ReparseClass::Plain => {}
            }
        }
        let folded = fold(name);
        if parent.depth == 0
            && matches!(
                folded.as_str(),
                "pagefile.sys" | "hiberfil.sys" | "swapfile.sys"
            )
        {
            return ContentDecision::Excluded {
                reason: ReasonCode::SwapOrHibernation,
                rule: "root-swap-file",
            };
        }
        if attributes.has(FileAttributes::OFFLINE)
            || attributes.has(FileAttributes::RECALL_ON_DATA_ACCESS)
        {
            // Reading would trigger a recall from tiered storage.
            return ContentDecision::Excluded {
                reason: ReasonCode::BinaryData,
                rule: "offline-attribute",
            };
        }
        if let Some(relative) = &relative {
            if let Some((index, r)) = self
                .rules
                .iter()
                .enumerate()
                .rev()
                .find(|(_, r)| r.matches(relative, self.case_sensitive))
            {
                return ContentDecision::Operator {
                    include: r.rule.include,
                    index,
                };
            }
        }
        if let Some((reason, rule)) = parent.inherited_content_exclusion {
            return ContentDecision::Excluded { reason, rule };
        }
        let ext = extension_of(name);
        match FileKind::from_extension(&ext) {
            FileKind::DiskImage => ContentDecision::Excluded {
                reason: ReasonCode::VmDiskImage,
                rule: "extension-disk-image",
            },
            FileKind::Media => ContentDecision::Excluded {
                reason: ReasonCode::MediaFile,
                rule: "extension-media",
            },
            FileKind::Binary | FileKind::Database => ContentDecision::Excluded {
                reason: ReasonCode::BinaryData,
                rule: "extension-binary",
            },
            // ZIP-family containers are candidates: the content job reads
            // their member inventory (ADR-0010), never member data.
            FileKind::Archive if eidos_domain::archive::archive_format(name).is_some() => {
                ContentDecision::Candidate
            }
            FileKind::Archive | FileKind::Document => ContentDecision::Unsupported,
            FileKind::Text
            | FileKind::Code
            | FileKind::Data
            | FileKind::Log
            | FileKind::Markup
            | FileKind::Unknown => ContentDecision::Candidate,
        }
    }

    pub fn child_path(parent: &PolicyCtx, name: &str) -> String {
        if parent.relative.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", parent.relative)
        }
    }

    pub fn is_protected(&self, relative: &str) -> bool {
        self.protected
            .iter()
            .any(|root| crate::exclusions::within(relative, root, self.case_sensitive))
    }
}

impl ContentDecision {
    /// Keep successful extraction (and binary-sniff outcomes) stable when
    /// metadata is unchanged. An explicit include may retry an unsupported file.
    pub fn changes_state(self, old: &str) -> bool {
        use eidos_domain::ContentState;
        let next = self.initial_state();
        if next == ContentState::Pending {
            old == "excluded"
                || (old == "unsupported" && matches!(self, Self::Operator { include: true, .. }))
        } else {
            old != next.as_str()
        }
    }

    pub fn initial_state(self) -> eidos_domain::ContentState {
        use eidos_domain::ContentState;
        match self {
            Self::Candidate | Self::Operator { include: true, .. } => ContentState::Pending,
            Self::Unsupported => ContentState::Unsupported,
            Self::Excluded { .. } | Self::Operator { include: false, .. } => ContentState::Excluded,
        }
    }
}

fn directory_rule(folded: &str, parent: &PolicyCtx) -> Option<(ReasonCode, &'static str)> {
    let at_root = parent.depth == 0;
    let parent_name = parent.name_folded.as_str();
    match folded {
        "$recycle.bin" | "recycler" | "recycled" if at_root => {
            Some((ReasonCode::RecycleBin, "root-recycle-bin"))
        }
        "system volume information" if at_root => Some((
            ReasonCode::SystemVolumeInformation,
            "root-system-volume-information",
        )),
        "node_modules" | "bower_components" | "jspm_packages" => {
            Some((ReasonCode::DependencyCache, "dir-node-modules"))
        }
        "site-packages" | ".venv" | "venv" | ".tox" => {
            Some((ReasonCode::DependencyCache, "dir-python-env"))
        }
        "__pycache__" | ".cache" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" => {
            Some((ReasonCode::KnownCache, "dir-tool-cache"))
        }
        "packages" if parent_name == ".nuget" => {
            Some((ReasonCode::DependencyCache, "dir-nuget-packages"))
        }
        "registry" | "git" if parent_name == ".cargo" => {
            Some((ReasonCode::DependencyCache, "dir-cargo-registry"))
        }
        "objects" | "lfs" if parent_name == ".git" => {
            Some((ReasonCode::KnownCache, "dir-git-objects"))
        }
        "temp" | "tmp" if parent_name == "local" => {
            Some((ReasonCode::KnownCache, "dir-appdata-temp"))
        }
        "inetcache" | "webcache" | "code cache" | "gpucache" | "cache2" | "service worker" => {
            Some((ReasonCode::KnownCache, "dir-browser-cache"))
        }
        "winsxs" | "servicing" if parent_name == "windows" => {
            Some((ReasonCode::PackageArtifact, "dir-windows-servicing"))
        }
        "installer" if parent_name == "windows" => {
            Some((ReasonCode::PackageArtifact, "dir-windows-installer"))
        }
        _ => None,
    }
}

pub fn fold(name: &str) -> String {
    name.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_is_not_excluded_by_name() {
        let p = PolicyEngine::new();
        let root = PolicyCtx::root();
        let bin = p.directory("bin", &root);
        assert!(bin.inherited_content_exclusion.is_none());
        assert_eq!(
            p.file("tool.cs", FileAttributes::default(), 0, &bin),
            ContentDecision::Candidate
        );
    }

    #[test]
    fn recycle_bin_only_at_root() {
        let p = PolicyEngine::new();
        let root = PolicyCtx::root();
        let rb = p.directory("$RECYCLE.BIN", &root);
        assert_eq!(
            rb.inherited_content_exclusion,
            Some((ReasonCode::RecycleBin, "root-recycle-bin"))
        );
        let deeper = p.directory("sub", &rb);
        assert!(deeper.inherited_content_exclusion.is_some(), "inherits");
        let tools = p.directory("Tools", &root);
        let nested = p.directory("$RECYCLE.BIN", &tools);
        assert!(nested.inherited_content_exclusion.is_none());
    }

    #[test]
    fn file_rules() {
        let p = PolicyEngine::new();
        let root = PolicyCtx::root();
        assert!(matches!(
            p.file("disk.vhdx", FileAttributes::default(), 0, &root),
            ContentDecision::Excluded {
                reason: ReasonCode::VmDiskImage,
                ..
            }
        ));
        assert!(matches!(
            p.file("pagefile.sys", FileAttributes::default(), 0, &root),
            ContentDecision::Excluded {
                reason: ReasonCode::SwapOrHibernation,
                ..
            }
        ));
        let sub = p.directory("x", &root);
        // Not at root: a file merely named pagefile.sys is a binary by extension.
        assert!(matches!(
            p.file("pagefile.sys", FileAttributes::default(), 0, &sub),
            ContentDecision::Excluded {
                reason: ReasonCode::BinaryData,
                ..
            }
        ));
        assert_eq!(
            p.file("report.pdf", FileAttributes::default(), 0, &root),
            ContentDecision::Unsupported
        );
        assert_eq!(
            p.file("notes.md", FileAttributes::default(), 0, &root),
            ContentDecision::Candidate
        );
        assert_eq!(
            p.file("tool.zip", FileAttributes::default(), 0, &root),
            ContentDecision::Candidate,
            "ZIP containers are manifest candidates"
        );
        assert_eq!(
            p.file("backup.7z", FileAttributes::default(), 0, &root),
            ContentDecision::Unsupported
        );
        assert_eq!(
            p.file("README", FileAttributes::default(), 0, &root),
            ContentDecision::Candidate
        );
    }

    #[test]
    fn context_sensitive_dirs() {
        let p = PolicyEngine::new();
        let root = PolicyCtx::root();
        let nuget = p.directory(".nuget", &root);
        let packages = p.directory("packages", &nuget);
        assert_eq!(
            packages.inherited_content_exclusion.map(|x| x.0),
            Some(ReasonCode::DependencyCache)
        );
        let other = p.directory("packages", &root);
        assert!(other.inherited_content_exclusion.is_none());
    }
}
