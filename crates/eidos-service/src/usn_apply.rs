//! USN record → [`ChangeEvent`] translation.
//!
//! Records are coalesced per file reference number in USN order so that the
//! *final* state wins, then each affected file is re-read by ID. The record
//! supplies the (parent, name) topology; the snapshot supplies sizes and
//! timestamps. Out-of-scope files (parents unknown to the catalog and not
//! created in this batch) are skipped before any I/O.

use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
use eidos_catalog::Catalog;
use eidos_domain::SourceId;
use eidos_scanner::usn::{
    hard_link_names, snapshot_by_id, snapshot_path, FileSnapshot, UsnRecord, VolumeHandle,
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE,
    USN_REASON_RENAME_OLD_NAME,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TranslateStats {
    pub records: u64,
    pub files: u64,
    pub snapshots: u64,
    pub vanished: u64,
    pub out_of_scope: u64,
    pub link_resyncs: u64,
    pub io_errors: u64,
}

pub struct Translator<'a> {
    pub vol: &'a VolumeHandle,
    pub volume_serial: u64,
    pub catalog: &'a Catalog,
    pub source_id: SourceId,
}

#[derive(Default)]
struct Acc {
    reasons: u32,
    old_names: Vec<(u128, String)>,
    latest: Option<(u128, String)>,
    deleted: bool,
    is_dir: bool,
}

impl<'a> Translator<'a> {
    fn key(&self, frn: u128) -> NativeKey {
        NativeKey {
            volume_serial: self.volume_serial,
            id: frn,
        }
    }

    fn in_scope(&self, parent_frn: u128, batch_dirs: &HashSet<u128>) -> bool {
        if batch_dirs.contains(&parent_frn) {
            return true;
        }
        self.catalog
            .object_by_native(self.source_id, self.key(parent_frn))
            .ok()
            .flatten()
            .is_some()
    }

    pub fn translate(&self, records: &[UsnRecord]) -> (Vec<ChangeEvent>, TranslateStats) {
        let mut stats = TranslateStats {
            records: records.len() as u64,
            ..Default::default()
        };
        let mut order: Vec<u128> = Vec::new();
        let mut accs: HashMap<u128, Acc> = HashMap::new();
        for r in records {
            let acc = accs.entry(r.frn).or_insert_with(|| {
                order.push(r.frn);
                Acc::default()
            });
            acc.reasons |= r.reason;
            acc.is_dir = r.is_directory();
            if r.has(USN_REASON_RENAME_OLD_NAME) {
                acc.old_names.push((r.parent_frn, r.name.clone()));
            } else {
                acc.latest = Some((r.parent_frn, r.name.clone()));
            }
            if r.has(USN_REASON_FILE_DELETE) {
                acc.deleted = true;
            }
            if r.has(USN_REASON_FILE_CREATE) {
                acc.deleted = false;
            }
        }
        stats.files = order.len() as u64;

        let mut events = Vec::new();
        let mut batch_dirs: HashSet<u128> = HashSet::new();
        for frn in order {
            let acc = accs.remove(&frn).expect("present");
            // Old names: unlink when the old parent is in scope.
            let mut any_in_scope = false;
            for (p, n) in &acc.old_names {
                if self.in_scope(*p, &batch_dirs) {
                    any_in_scope = true;
                    events.push(ChangeEvent::Unlink {
                        parent: self.key(*p),
                        name: n.clone(),
                    });
                }
            }
            if acc.deleted {
                // Only meaningful if the object is known; apply_changes skips
                // unknown keys cheaply.
                events.push(ChangeEvent::Delete {
                    object: self.key(frn),
                });
                continue;
            }
            let latest_in_scope = acc
                .latest
                .as_ref()
                .map(|(p, _)| self.in_scope(*p, &batch_dirs))
                .unwrap_or(false);
            if !latest_in_scope && !any_in_scope && acc.reasons & USN_REASON_HARD_LINK_CHANGE == 0 {
                stats.out_of_scope += 1;
                continue;
            }
            let snap = match snapshot_by_id(self.vol, frn) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    stats.vanished += 1;
                    events.push(ChangeEvent::Delete {
                        object: self.key(frn),
                    });
                    continue;
                }
                Err(e) => {
                    stats.io_errors += 1;
                    tracing::debug!(frn, error = %e, "snapshot by id failed");
                    continue;
                }
            };
            stats.snapshots += 1;
            let object = to_snapshot(&snap);
            if acc.reasons & USN_REASON_HARD_LINK_CHANGE != 0 && !snap.kind.is_directory_like() {
                if let Some(path) = &snap.path {
                    if self.resync_links(path, &object, &mut events, &batch_dirs) {
                        stats.link_resyncs += 1;
                        continue;
                    }
                }
            }
            if let Some((p, n)) = acc.latest {
                if self.in_scope(p, &batch_dirs) {
                    if acc.is_dir || snap.kind.is_directory_like() {
                        batch_dirs.insert(frn);
                    }
                    events.push(ChangeEvent::Link {
                        parent: self.key(p),
                        name: n,
                        snapshot: object,
                    });
                }
            }
        }
        (events, stats)
    }

    /// Emit Link events for every current hard-link name and Unlink events
    /// for catalog entries that no longer exist. Returns false when link
    /// enumeration failed (caller falls back to the single latest name).
    fn resync_links(
        &self,
        path: &str,
        object: &ObjectSnapshot,
        events: &mut Vec<ChangeEvent>,
        batch_dirs: &HashSet<u128>,
    ) -> bool {
        let names = match hard_link_names(std::path::Path::new(path)) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let root = self.vol.root.trim_end_matches('\\');
        let mut current: HashSet<(NativeKey, String)> = HashSet::new();
        for link in names {
            let full = format!("{root}{link}");
            let p = std::path::Path::new(&full);
            let (dir, name) = match (p.parent(), p.file_name()) {
                (Some(d), Some(n)) => (d.to_path_buf(), n.to_string_lossy().into_owned()),
                _ => continue,
            };
            let parent_snap = match snapshot_path(&dir) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let pk = NativeKey::from(parent_snap.native);
            if !self.in_scope(pk.id, batch_dirs) {
                continue;
            }
            current.insert((pk, name.clone()));
            events.push(ChangeEvent::Link {
                parent: pk,
                name,
                snapshot: object.clone(),
            });
        }
        // Unlink catalog entries that are no longer among the live names.
        if let Ok(Some(id)) = self
            .catalog
            .object_by_native(self.source_id, NativeKey::from(object.native))
        {
            if let Ok(entries) = self.catalog.entries_for_object(id) {
                for e in entries {
                    let parent_id = match e.parent_id {
                        Some(p) => p,
                        None => continue,
                    };
                    let parent_native = self
                        .catalog
                        .get_object(parent_id)
                        .ok()
                        .flatten()
                        .and_then(|o| o.native);
                    if let Some(pn) = parent_native {
                        let pk = NativeKey::from(pn);
                        if !current.contains(&(pk, e.name.clone())) {
                            events.push(ChangeEvent::Unlink {
                                parent: pk,
                                name: e.name,
                            });
                        }
                    }
                }
            }
        }
        true
    }
}

pub fn to_snapshot(s: &FileSnapshot) -> ObjectSnapshot {
    ObjectSnapshot {
        native: s.native,
        kind: s.kind,
        attributes: s.attributes,
        size: s.size,
        allocated: s.allocated,
        link_count: s.link_count,
        created: s.created,
        modified: s.modified,
        changed: s.changed,
        accessed: s.accessed,
        reparse_tag: s.reparse_tag,
    }
}
