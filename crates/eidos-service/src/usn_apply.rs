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
use eidos_scanner::{ScanError, ScanErrorKind};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TranslateStats {
    pub records: u64,
    pub files: u64,
    pub snapshots: u64,
    pub vanished: u64,
    pub out_of_scope: u64,
    pub link_resyncs: u64,
    /// Snapshots that failed transiently. The batch is retried from the same
    /// position rather than acknowledged.
    pub io_errors: u64,
    /// Snapshots that cannot succeed by replaying the record (denied,
    /// unsupported). Skipped like an unlistable file in a scan.
    pub unreadable: u64,
}

impl TranslateStats {
    /// Record one failed `snapshot_by_id`. Only a failure that replaying the
    /// same record cannot fix may be skipped: a single protected file inside
    /// an indexed root would otherwise stall the live feed until the journal
    /// wrapped and abort every overlap replay. Everything else keeps the
    /// checkpoint, because acknowledging it would drop a change that a retry
    /// could still have read.
    fn record_snapshot_failure(&mut self, frn: u128, error: &ScanError) {
        // Exhaustive on purpose: a new ScanErrorKind has to make this choice
        // deliberately rather than default into dropping an update.
        let permanent = match error.kind {
            ScanErrorKind::AccessDenied
            | ScanErrorKind::NotFound
            | ScanErrorKind::Unsupported
            | ScanErrorKind::InvalidName => true,
            // Transient is retryable by definition. Other is every code the
            // classifier does not know, which on Windows includes recoverable
            // ones such as ERROR_IO_DEVICE and ERROR_OPERATION_ABORTED, so it
            // is retried rather than acknowledged.
            ScanErrorKind::Transient | ScanErrorKind::Other => false,
        };
        if permanent {
            self.unreadable += 1;
        } else {
            self.io_errors += 1;
        }
        tracing::debug!(frn, error = %error, permanent, "snapshot by id failed");
    }

    fn ensure_complete(&self) -> eidos_catalog::Result<()> {
        if self.io_errors > 0 {
            return Err(eidos_catalog::CatalogError::InvalidState(format!(
                "USN batch has {} unread snapshots; retaining checkpoint for retry",
                self.io_errors
            )));
        }
        Ok(())
    }
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

    fn in_scope(
        &self,
        parent_frn: u128,
        batch_dirs: &HashSet<u128>,
    ) -> eidos_catalog::Result<bool> {
        if batch_dirs.contains(&parent_frn) {
            return Ok(true);
        }
        match self
            .catalog
            .object_by_native(self.source_id, self.key(parent_frn))?
        {
            Some(id) => Ok(!self.catalog.object_is_protected(self.source_id, id)?),
            None => Ok(false),
        }
    }

    pub fn translate(
        &self,
        records: &[UsnRecord],
    ) -> eidos_catalog::Result<(Vec<ChangeEvent>, TranslateStats)> {
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
                if self.in_scope(*p, &batch_dirs)? {
                    any_in_scope = true;
                    events.push(ChangeEvent::Unlink {
                        parent: self.key(*p),
                        name: n.clone(),
                    });
                }
            }
            if acc.deleted {
                // A volume journal includes other roots and our own stores.
                // Unknown deletes are not source events: emitting them would
                // force a checkpoint write for unrelated temporary-file churn.
                if let Some(event) = known_deletion(self.catalog, self.source_id, self.key(frn))? {
                    events.push(event);
                } else {
                    stats.out_of_scope += 1;
                }
                continue;
            }
            let latest_in_scope = acc
                .latest
                .as_ref()
                .map(|(p, _)| self.in_scope(*p, &batch_dirs))
                .transpose()?
                .unwrap_or(false);
            if !latest_in_scope && !any_in_scope && acc.reasons & USN_REASON_HARD_LINK_CHANGE == 0 {
                stats.out_of_scope += 1;
                continue;
            }
            let snap = match snapshot_by_id(self.vol, frn) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    // Gone between the record and the snapshot. As on the
                    // deleted arm, only an identity the source knows is a
                    // change event; anything else is temporary-file churn
                    // whose event would force a checkpoint write.
                    stats.vanished += 1;
                    if let Some(event) =
                        known_deletion(self.catalog, self.source_id, self.key(frn))?
                    {
                        events.push(event);
                    }
                    continue;
                }
                Err(e) => {
                    stats.record_snapshot_failure(frn, &e);
                    continue;
                }
            };
            stats.snapshots += 1;
            let object = to_snapshot(&snap);
            if acc.reasons & USN_REASON_HARD_LINK_CHANGE != 0 && !snap.kind.is_directory_like() {
                if let Some(path) = &snap.path {
                    if self.resync_links(path, &object, &mut events, &batch_dirs)? {
                        stats.link_resyncs += 1;
                        continue;
                    }
                }
            }
            if let Some((p, n)) = acc.latest {
                if self.in_scope(p, &batch_dirs)? {
                    let protected_path = snap
                        .path
                        .as_ref()
                        .map(|path| self.catalog.path_is_protected(self.source_id, path))
                        .transpose()?
                        .unwrap_or(false);
                    if (acc.is_dir || snap.kind.is_directory_like()) && !protected_path {
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
        // Both live watching and overlap replay must reject partial snapshots.
        // An empty event list is not proof that an unread file is irrelevant.
        stats.ensure_complete()?;
        Ok((events, stats))
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
    ) -> eidos_catalog::Result<bool> {
        let names = match hard_link_names(std::path::Path::new(path)) {
            Ok(n) => n,
            Err(_) => return Ok(false),
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
            if !self.in_scope(pk.id, batch_dirs)? {
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
        Ok(true)
    }
}

fn known_deletion(
    catalog: &Catalog,
    source: SourceId,
    key: NativeKey,
) -> eidos_catalog::Result<Option<ChangeEvent>> {
    Ok(catalog
        .object_by_native(source, key)?
        .map(|_| ChangeEvent::Delete { object: key }))
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

#[cfg(test)]
mod deletion_tests {
    use super::*;

    #[test]
    fn partial_snapshot_translation_cannot_be_acknowledged_as_complete() {
        assert!(TranslateStats::default().ensure_complete().is_ok());
        assert!(TranslateStats {
            io_errors: 1,
            ..Default::default()
        }
        .ensure_complete()
        .is_err());
    }

    #[test]
    fn only_unfixable_snapshot_failures_may_be_skipped() {
        let error = |kind, code| ScanError::new(kind, code, "fixture", std::path::Path::new("V"));
        let mut stats = TranslateStats::default();
        // ACCESS_DENIED, PRIVILEGE_NOT_HELD, NOT_SUPPORTED, unrepresentable
        // name: replaying the record produces the same failure forever.
        for (kind, code) in [
            (ScanErrorKind::AccessDenied, 5),
            (ScanErrorKind::AccessDenied, 1314),
            (ScanErrorKind::Unsupported, 50),
            (ScanErrorKind::NotFound, 2),
            (ScanErrorKind::InvalidName, 0),
        ] {
            stats.record_snapshot_failure(1, &error(kind, code));
        }
        assert_eq!(stats.unreadable, 5);
        assert_eq!(stats.io_errors, 0);
        assert!(
            stats.ensure_complete().is_ok(),
            "a permanently unreadable object must not stall the feed or abort replay"
        );
        // SHARING_VIOLATION is retryable by classification, and an
        // unclassified code must not be assumed permanent: ERROR_IO_DEVICE and
        // ERROR_OPERATION_ABORTED reach Other and can succeed on retry.
        for (kind, code) in [
            (ScanErrorKind::Transient, 32),
            (ScanErrorKind::Other, 1117),
            (ScanErrorKind::Other, 995),
        ] {
            let mut retryable = TranslateStats::default();
            retryable.record_snapshot_failure(2, &error(kind, code));
            assert_eq!(retryable.io_errors, 1, "os {code} must retain the position");
            assert_eq!(retryable.unreadable, 0);
            assert!(retryable.ensure_complete().is_err());
        }
    }

    #[test]
    fn unclassified_windows_codes_land_in_the_retryable_bucket() {
        // The choice above is only safe if these codes really do classify as
        // Other rather than as something the permanent arm would swallow.
        use eidos_scanner::classify_os_error;
        for code in [1117i32, 995, 23, 1392] {
            assert_eq!(
                classify_os_error(code, std::io::ErrorKind::Other),
                ScanErrorKind::Other,
                "os {code} is unclassified and must stay retryable"
            );
        }
    }

    #[test]
    fn a_denied_snapshot_is_classified_from_the_real_open_failure() {
        // The classification must follow snapshot_by_id's own mapping, not a
        // guess: only NOT_FOUND-like codes reach the vanished arm.
        use eidos_scanner::classify_os_error;
        for code in [5i32, 1314, 1920] {
            assert_eq!(
                classify_os_error(code, std::io::ErrorKind::Other),
                ScanErrorKind::AccessDenied,
                "os {code} must be permanent"
            );
        }
        assert_eq!(
            classify_os_error(32, std::io::ErrorKind::Other),
            ScanErrorKind::Transient
        );
    }

    #[test]
    fn an_indexed_identity_still_emits_its_deletion() {
        use eidos_catalog::{
            scan::{run_scan, RunScanOptions},
            NewSource,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("known.txt"), b"fixture").unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host_id = catalog.ensure_host("fixture", "windows").unwrap();
        let source = catalog
            .add_source(&NewSource {
                host_id,
                name: "fixture".into(),
                kind: eidos_domain::SourceKind::WindowsLocal,
                root_path: root.to_string_lossy().into_owned(),
                aliases: vec![],
            })
            .unwrap();
        run_scan(
            &catalog,
            source,
            eidos_scanner::default_lister().as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
        let object = catalog
            .resolve_relative(source, "known.txt")
            .unwrap()
            .unwrap();
        let key = NativeKey::from(catalog.get_object(object).unwrap().unwrap().native.unwrap());
        assert!(
            matches!(known_deletion(&catalog, source, key).unwrap(), Some(ChangeEvent::Delete { object }) if object == key)
        );
    }

    #[test]
    fn deletes_absent_from_the_source_do_not_become_feed_events() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        for id in 1..100 {
            assert!(known_deletion(
                &catalog,
                SourceId(1),
                NativeKey {
                    volume_serial: 7,
                    id
                }
            )
            .unwrap()
            .is_none());
        }
    }

    #[test]
    fn a_failed_catalog_lookup_is_not_confused_with_an_unrelated_delete() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        // Fault injection in this disposable, empty catalog only.
        catalog
            .with_writer(|conn| {
                conn.execute_batch("ALTER TABLE objects RENAME TO unavailable_objects")?;
                Ok(())
            })
            .unwrap();
        assert!(known_deletion(
            &catalog,
            SourceId(1),
            NativeKey {
                volume_serial: 7,
                id: 1
            }
        )
        .is_err());
    }
}
