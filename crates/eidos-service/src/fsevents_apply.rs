//! FSEvents path notification → [`ChangeEvent`] translation.
//!
//! A USN record describes a change; an FSEvents notification only names a path
//! that changed at least once. Translation therefore re-reads the filesystem
//! and lets it, not the notification, say what is true now:
//!
//! - a path that still exists becomes a `Link` under its parent's identity,
//!   plus `Unlink`s for any catalog entry that used to name the same object
//!   somewhere else, which is how a rename or a move is recognised without a
//!   paired "old name" record;
//! - a path that is gone is resolved against the catalog and becomes an
//!   `Unlink` (a file may still have other links) or a `Delete` (a directory
//!   takes its subtree with it);
//! - a directory that the catalog has never seen is enumerated, because a
//!   subtree *moved into* the tree generates one notification for its new root
//!   and none for the children that came with it.
//!
//! Notifications are processed shallowest-first so a parent is linked before
//! the children that need its identity.

use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
use eidos_catalog::Catalog;
use eidos_domain::{ObjectKind, SourceId};
use eidos_scanner::fsevents::PathChange;
use eidos_scanner::{walk, DirectoryLister, RawEntry, WalkOptions};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Entries a single batch may enumerate on behalf of moved-in subtrees before
/// the batch is abandoned in favour of a reconciliation scan. A move of an
/// entire home directory should become one scan, not one enormous batch.
const MAX_EXPANDED_ENTRIES: usize = 50_000;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TranslateStats {
    pub paths: u64,
    pub snapshots: u64,
    pub removed: u64,
    pub relinked: u64,
    pub expanded_directories: u64,
    pub out_of_scope: u64,
    /// Failures that will not resolve by trying again (denied, malformed).
    pub io_errors: u64,
    /// Failures worth retrying. The cursor must not advance past a batch that
    /// hit one, or a change would be durably acknowledged without ever having
    /// been applied.
    pub retryable_errors: u64,
    /// The batch touched more than one translation can safely describe; the
    /// caller must reconcile by enumeration instead of applying it.
    pub needs_rescan: bool,
}

pub struct PathTranslator<'a> {
    pub lister: &'a dyn DirectoryLister,
    pub catalog: &'a Catalog,
    pub source_id: SourceId,
    /// Source root, already canonicalised the way FSEvents reports paths.
    pub root: &'a Path,
}

impl PathTranslator<'_> {
    /// Native key of the object at `path` as the filesystem reports it now.
    fn key_of(&self, path: &Path) -> Option<NativeKey> {
        self.lister
            .stat(path)
            .ok()
            .and_then(|entry| entry.native_id)
            .map(NativeKey::from)
    }

    /// Whether the catalog can attach children to this parent: it either knows
    /// the object already or this batch is about to create it.
    fn in_scope(&self, parent: NativeKey, batch_dirs: &HashSet<NativeKey>) -> bool {
        batch_dirs.contains(&parent)
            || self
                .catalog
                .object_by_native(self.source_id, parent)
                .ok()
                .flatten()
                .is_some()
    }

    fn snapshot(&self, entry: &RawEntry) -> Option<ObjectSnapshot> {
        Some(ObjectSnapshot {
            native: entry.native_id?,
            kind: entry.kind,
            attributes: entry.attributes,
            size: entry.size,
            allocated: entry.allocated.unwrap_or(entry.size),
            // The catalog recomputes link counts from its own entries after
            // every batch; this is only the initial value for a new object.
            link_count: 1,
            created: entry.created,
            modified: entry.modified,
            changed: entry.changed,
            accessed: entry.accessed,
            reparse_tag: entry.reparse_tag,
        })
    }

    /// Path relative to the source root, or `None` when the notification is
    /// for something outside the source.
    fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(self.root).ok().map(Path::to_path_buf)
    }

    /// Unlink every catalog entry that names this object somewhere other than
    /// where it now lives. This is what turns "the new path changed" into a
    /// completed rename, and it also repairs hard links that were removed
    /// while the agent was not watching.
    fn unlink_stale_entries(
        &self,
        object: NativeKey,
        current_parent: NativeKey,
        current_name: &str,
        events: &mut Vec<ChangeEvent>,
    ) -> bool {
        let Ok(Some(id)) = self.catalog.object_by_native(self.source_id, object) else {
            return false;
        };
        let Ok(entries) = self.catalog.entries_for_object(id) else {
            return false;
        };
        let mut unlinked = false;
        for entry in entries {
            let Some(parent_id) = entry.parent_id else {
                continue;
            };
            let Some(parent_native) = self
                .catalog
                .get_object(parent_id)
                .ok()
                .flatten()
                .and_then(|o| o.native)
            else {
                continue;
            };
            let parent_key = NativeKey::from(parent_native);
            if parent_key == current_parent && entry.name == current_name {
                continue;
            }
            // The entry names this object elsewhere. If that path still
            // exists it is a genuine hard link and must stay.
            if self.entry_names_object(parent_id, &entry.name, object) {
                continue;
            }
            events.push(ChangeEvent::Unlink {
                parent: parent_key,
                name: entry.name,
            });
            unlinked = true;
        }
        unlinked
    }

    /// Whether the catalog's `(parent, name)` entry still names this object on
    /// disk. This is what separates a stale entry from a live hard link, so it
    /// has to be exact about the name rather than about the path.
    ///
    /// A path lookup alone is not exact enough on a case-insensitive volume:
    /// after `mv Report.txt report.txt` the old path still resolves — to the
    /// very same file — and believing it would leave the catalog holding both
    /// spellings of one object. When the path resolves to this object, the
    /// parent directory is read once to see whether the entry is really still
    /// spelled that way.
    fn entry_names_object(
        &self,
        parent_id: eidos_domain::ObjectId,
        name: &str,
        object: NativeKey,
    ) -> bool {
        let Ok(Some(parent_path)) = self.catalog.render_path(parent_id) else {
            // Without a path the safest answer is "still there": a spurious
            // unlink loses data, while a missed one is repaired by the next
            // reconciliation.
            return true;
        };
        let parent_path = PathBuf::from(parent_path);
        let Ok(entry) = self.lister.stat(&parent_path.join(name)) else {
            return false;
        };
        match entry.native_id.map(NativeKey::from) {
            // Some other object answers to that name now, so this object's
            // entry under it is stale whatever the spelling.
            Some(found) if found != object => false,
            None => true,
            Some(_) => match std::fs::read_dir(&parent_path) {
                Ok(children) => children
                    .filter_map(Result::ok)
                    .any(|child| child.file_name().to_string_lossy() == name),
                // The directory became unreadable between the two calls;
                // keeping the entry is the conservative answer.
                Err(_) => true,
            },
        }
    }

    /// Enumerate a directory that the catalog has never seen, emitting a
    /// `Link` for everything inside it. Returns `false` when the subtree is
    /// too large to describe as one batch.
    fn expand_directory(
        &self,
        path: &Path,
        events: &mut Vec<ChangeEvent>,
        batch_dirs: &mut HashSet<NativeKey>,
        stats: &mut TranslateStats,
    ) -> bool {
        let mut budget = MAX_EXPANDED_ENTRIES;
        let mut overflowed = false;
        walk(
            path,
            self.lister,
            &WalkOptions {
                threads: 2,
                ..Default::default()
            },
            |event| {
                if overflowed {
                    return;
                }
                let children = match &event.result {
                    Ok(children) => children,
                    Err(e) if e.is_retryable() => {
                        stats.retryable_errors += 1;
                        return;
                    }
                    Err(_) => {
                        stats.io_errors += 1;
                        return;
                    }
                };
                let Some(parent) = self.key_of(&event.path) else {
                    stats.io_errors += 1;
                    return;
                };
                stats.expanded_directories += 1;
                for child in children {
                    if budget == 0 {
                        overflowed = true;
                        return;
                    }
                    budget -= 1;
                    let Some(snapshot) = self.snapshot(child) else {
                        continue;
                    };
                    if child.kind == ObjectKind::Directory {
                        batch_dirs.insert(NativeKey::from(snapshot.native));
                    }
                    stats.snapshots += 1;
                    events.push(ChangeEvent::Link {
                        parent,
                        name: child.name.clone(),
                        snapshot,
                    });
                }
            },
        );
        !overflowed
    }

    /// One notification for a path that still exists.
    fn present(
        &self,
        path: &Path,
        entry: RawEntry,
        relinked: &mut HashSet<NativeKey>,
        events: &mut Vec<ChangeEvent>,
        batch_dirs: &mut HashSet<NativeKey>,
        stats: &mut TranslateStats,
    ) {
        let Some(parent_path) = path.parent() else {
            return;
        };
        let Some(parent) = self.key_of(parent_path) else {
            stats.out_of_scope += 1;
            return;
        };
        if !self.in_scope(parent, batch_dirs) {
            stats.out_of_scope += 1;
            return;
        }
        let Some(snapshot) = self.snapshot(&entry) else {
            stats.io_errors += 1;
            return;
        };
        let object = NativeKey::from(snapshot.native);
        let known = self
            .catalog
            .object_by_native(self.source_id, object)
            .ok()
            .flatten()
            .is_some();
        if self.unlink_stale_entries(object, parent, &entry.name, events) {
            stats.relinked += 1;
        }
        let is_directory = entry.kind == ObjectKind::Directory;
        if is_directory {
            batch_dirs.insert(object);
        }
        relinked.insert(object);
        stats.snapshots += 1;
        events.push(ChangeEvent::Link {
            parent,
            name: entry.name,
            snapshot,
        });
        // A directory the catalog has never seen may have arrived with its
        // whole subtree; nothing inside it generated a notification of its
        // own, so it has to be read.
        if is_directory && !known && !self.expand_directory(path, events, batch_dirs, stats) {
            stats.needs_rescan = true;
        }
    }

    /// One notification for a path that no longer exists.
    ///
    /// `relinked` holds the objects this batch has already found living
    /// somewhere else. A path that vanished *because its object moved* is a
    /// rename, and the move's own notification has already re-linked it —
    /// deleting it here would take a live subtree with it.
    fn absent(
        &self,
        path: &Path,
        relinked: &HashSet<NativeKey>,
        events: &mut Vec<ChangeEvent>,
        stats: &mut TranslateStats,
    ) {
        let Some(relative) = self.relative(path) else {
            return;
        };
        let relative = relative.to_string_lossy().into_owned();
        let Ok(Some(object_id)) = self.catalog.resolve_relative(self.source_id, &relative) else {
            // Never catalogued, or already removed by an earlier batch.
            return;
        };
        let Ok(Some(object)) = self.catalog.get_object(object_id) else {
            return;
        };
        let Some(native) = object.native else {
            return;
        };
        if relinked.contains(&NativeKey::from(native)) {
            // Moved, not removed. The `Link` for its new path already
            // unlinked this entry.
            return;
        }
        stats.removed += 1;
        if object.kind.is_directory_like() {
            // The subtree goes with it; individual children produce no
            // notifications of their own when a directory is removed.
            events.push(ChangeEvent::Delete {
                object: NativeKey::from(native),
            });
            return;
        }
        let Some(parent_path) = path.parent() else {
            return;
        };
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        let parent = match self.key_of(parent_path) {
            Some(key) => key,
            None => {
                // The parent is gone too; its own notification (or the
                // subtree delete above) covers this entry.
                return;
            }
        };
        events.push(ChangeEvent::Unlink { parent, name });
    }

    /// Translate one FSEvents batch. Paths are deduplicated and ordered
    /// shallowest-first so that a parent exists before its children are
    /// attached to it.
    pub fn translate(&self, changes: &[PathChange]) -> (Vec<ChangeEvent>, TranslateStats) {
        let mut stats = TranslateStats::default();
        let mut ordered: BTreeMap<(usize, PathBuf), ()> = BTreeMap::new();
        for change in changes {
            if self.relative(&change.path).is_none() {
                stats.out_of_scope += 1;
                continue;
            }
            if change.path == self.root {
                // The root's own entry belongs to the source, not to a parent
                // inside it; its aggregates are recomputed by the catalog.
                continue;
            }
            ordered.insert((change.path.components().count(), change.path.clone()), ());
        }
        stats.paths = ordered.len() as u64;
        let mut events = Vec::new();
        let mut batch_dirs: HashSet<NativeKey> = HashSet::new();
        let mut relinked: HashSet<NativeKey> = HashSet::new();
        let mut absent: Vec<PathBuf> = Vec::new();
        // Existing paths first: they establish where each object lives now,
        // which is what tells a rename apart from a deletion.
        for (_, path) in ordered.into_keys() {
            match self.lister.stat(&path) {
                Ok(entry) => self.present(
                    &path,
                    entry,
                    &mut relinked,
                    &mut events,
                    &mut batch_dirs,
                    &mut stats,
                ),
                Err(e) if e.kind == eidos_scanner::ScanErrorKind::NotFound => absent.push(path),
                Err(e) => {
                    if e.is_retryable() {
                        stats.retryable_errors += 1;
                    } else {
                        stats.io_errors += 1;
                    }
                    tracing::debug!(path = %path.display(), error = %e, "could not read a changed path");
                }
            }
            if stats.needs_rescan {
                return (events, stats);
            }
        }
        for path in absent {
            self.absent(&path, &relinked, &mut events, &mut stats);
        }
        (events, stats)
    }
}
