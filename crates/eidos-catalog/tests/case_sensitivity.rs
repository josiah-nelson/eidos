//! Path resolution follows the volume's case semantics.
//!
//! Ingestion has always stored the exact name, so a case-sensitive volume can
//! hold `Report.txt` and `report.txt` side by side. The entries are built
//! through the change feed rather than a real directory, because a host whose
//! own filesystem is case-insensitive cannot create such a pair.

use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{
    FileAttributes, IdentityConfidence, NativeIdentity, ObjectKind, SourceId, SourceKind, UnixNanos,
};
use eidos_scanner::{DriveType, NativeFeed, VolumeInfo};

struct Fx {
    _dir: tempfile::TempDir,
    catalog: std::sync::Arc<Catalog>,
    source: SourceId,
    serial: u64,
    next_id: u128,
}

fn fixture(case_sensitive: Option<bool>) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("h", "test").unwrap();
    let source = catalog
        .add_source(&NewSource {
            host_id: host,
            name: "fx".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    let lister = eidos_scanner::default_lister();
    run_scan(
        &catalog,
        source,
        lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    let root_object = catalog
        .get_source(source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let serial = catalog
        .get_object(root_object)
        .unwrap()
        .unwrap()
        .native
        .expect("root identity")
        .volume_serial;
    catalog
        .upsert_volume(
            host,
            source,
            &VolumeInfo {
                volume_serial: serial,
                filesystem: "test".into(),
                volume_name: "fixture".into(),
                drive_type: DriveType::Fixed,
                case_sensitive,
                native_feed: NativeFeed::None,
                bytes_per_cluster: 4096,
                volume_root: root.display().to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    Fx {
        _dir: dir,
        catalog,
        source,
        serial,
        next_id: 0xFFFF_0000_0000_0000_0000_0000_0000_0000,
    }
}

impl Fx {
    fn root_key(&self) -> NativeKey {
        let root = self
            .catalog
            .get_source(self.source)
            .unwrap()
            .unwrap()
            .root_object_id
            .unwrap();
        NativeKey::from(
            self.catalog
                .get_object(root)
                .unwrap()
                .unwrap()
                .native
                .expect("root identity"),
        )
    }

    /// Link one file directly under the source root.
    fn add(&mut self, name: &str, size: u64) {
        self.next_id += 1;
        let snapshot = ObjectSnapshot {
            native: NativeIdentity::from_u128(
                self.serial,
                self.next_id,
                IdentityConfidence::Native,
            ),
            kind: ObjectKind::File,
            attributes: FileAttributes(0x20),
            size,
            allocated: size.div_ceil(4096) * 4096,
            link_count: 1,
            created: Some(UnixNanos::now()),
            modified: Some(UnixNanos::now()),
            changed: None,
            accessed: None,
            reparse_tag: 0,
        };
        let parent = self.root_key();
        self.catalog
            .apply_changes(
                self.source,
                &[ChangeEvent::Link {
                    parent,
                    name: name.to_string(),
                    snapshot,
                }],
                None,
            )
            .unwrap();
    }

    fn size_at(&self, rel: &str) -> Option<u64> {
        let id = self.catalog.resolve_relative(self.source, rel).unwrap()?;
        Some(self.catalog.get_object(id).unwrap().unwrap().size)
    }
}

#[test]
fn a_case_sensitive_volume_resolves_case_distinct_siblings_separately() {
    let mut fx = fixture(Some(true));
    fx.add("Report.txt", 100);
    fx.add("report.txt", 200);

    assert_eq!(fx.size_at("Report.txt"), Some(100));
    assert_eq!(fx.size_at("report.txt"), Some(200));
    assert_eq!(
        fx.size_at("REPORT.TXT"),
        None,
        "a name that exists in neither case must not match a sibling"
    );
}

#[test]
fn a_case_insensitive_volume_still_resolves_any_case() {
    let mut fx = fixture(Some(false));
    fx.add("Report.txt", 100);

    assert_eq!(fx.size_at("Report.txt"), Some(100));
    assert_eq!(fx.size_at("report.txt"), Some(100));
    assert_eq!(fx.size_at("REPORT.TXT"), Some(100));
}

#[test]
fn an_unprobed_volume_keeps_the_case_insensitive_default() {
    let mut fx = fixture(None);
    fx.add("Report.txt", 100);

    assert_eq!(
        fx.size_at("report.txt"),
        Some(100),
        "an unknown volume must behave the way it did before it was probed"
    );
}
