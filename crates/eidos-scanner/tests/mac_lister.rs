//! macOS `getattrlistbulk` adapter: agreement with the portable lister, the
//! facts only the native path can report, and the volume capability probe.
//!
//! These tests run on a temporary directory of the boot volume, so they assert
//! what any APFS volume must report rather than anything about this host.

#![cfg(target_os = "macos")]

use eidos_domain::{FileAttributes, ObjectKind};
use eidos_scanner::mac::MacLister;
use eidos_scanner::std_lister::StdLister;
use eidos_scanner::{DirectoryLister, DriveType, NativeFeed, RawEntry};
use std::collections::BTreeMap;
use std::path::Path;

fn by_name(entries: Vec<RawEntry>) -> BTreeMap<String, RawEntry> {
    entries.into_iter().map(|e| (e.name.clone(), e)).collect()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("bramble.txt"), b"twelve bytes").unwrap();
    std::fs::write(root.join(".hidden-by-name"), b"x").unwrap();
    std::fs::create_dir(root.join("cedar")).unwrap();
    std::fs::write(root.join("cedar/nested.bin"), vec![7u8; 5000]).unwrap();
    std::os::unix::fs::symlink(root.join("bramble.txt"), root.join("link-to-bramble")).unwrap();
    std::fs::write(root.join("readonly.txt"), b"locked").unwrap();
    let mut permissions = std::fs::metadata(root.join("readonly.txt"))
        .unwrap()
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(root.join("readonly.txt"), permissions).unwrap();
    dir
}

#[test]
fn the_native_lister_agrees_with_the_portable_one() {
    let dir = fixture();
    let native = by_name(MacLister::new().list(dir.path()).unwrap());
    let portable = by_name(StdLister.list(dir.path()).unwrap());

    assert_eq!(
        native.keys().collect::<Vec<_>>(),
        portable.keys().collect::<Vec<_>>(),
        "both listers must see the same children"
    );
    for (name, native_entry) in &native {
        let portable_entry = &portable[name];
        assert_eq!(native_entry.kind, portable_entry.kind, "kind of {name}");
        assert_eq!(native_entry.size, portable_entry.size, "size of {name}");
        assert_eq!(
            native_entry.native_id, portable_entry.native_id,
            "identity of {name}"
        );
        assert_eq!(
            native_entry.attributes.0, portable_entry.attributes.0,
            "attributes of {name}"
        );
        // Modification time is exact on APFS; the portable lister reads the
        // same value through `stat`.
        assert_eq!(
            native_entry.modified, portable_entry.modified,
            "modified of {name}"
        );
    }
}

#[test]
fn the_native_lister_reports_facts_the_portable_one_cannot() {
    let dir = fixture();
    let native = by_name(MacLister::new().list(dir.path()).unwrap());

    let file = &native["bramble.txt"];
    assert_eq!(file.kind, ObjectKind::File);
    assert_eq!(file.size, 12);
    assert!(
        file.allocated.is_some_and(|a| a >= file.size),
        "allocation size is only available from the native path: {:?}",
        file.allocated
    );
    assert!(file.changed.is_some(), "ctime is not visible to `stat`");
    assert!(file.created.is_some());

    let hidden = &native[".hidden-by-name"];
    assert!(
        hidden.attributes.has(FileAttributes::HIDDEN),
        "leading-dot names are hidden by macOS convention"
    );

    let link = &native["link-to-bramble"];
    assert_eq!(link.kind, ObjectKind::Reparse);
    assert!(link.attributes.is_reparse());
    assert!(!link.is_traversable_dir());

    let readonly = &native["readonly.txt"];
    assert!(readonly.attributes.has(FileAttributes::READONLY));

    let directory = &native["cedar"];
    assert!(directory.is_traversable_dir());
    assert_eq!(directory.size, 0);
}

#[test]
fn stat_matches_a_listing_of_the_same_object() {
    let dir = fixture();
    let lister = MacLister::new();
    let listed = by_name(lister.list(dir.path()).unwrap());
    for name in ["bramble.txt", "cedar", "link-to-bramble"] {
        let stated = lister.stat(&dir.path().join(name)).unwrap();
        let entry = &listed[name];
        assert_eq!(stated.name, entry.name);
        assert_eq!(stated.kind, entry.kind, "kind of {name}");
        assert_eq!(stated.size, entry.size, "size of {name}");
        assert_eq!(stated.native_id, entry.native_id, "identity of {name}");
        assert_eq!(
            stated.attributes.0, entry.attributes.0,
            "attributes of {name}"
        );
    }
}

#[test]
fn the_boot_volume_reports_apfs_capabilities() {
    let dir = fixture();
    let volume = MacLister::new().volume_info(dir.path()).unwrap();
    assert_eq!(volume.filesystem, "apfs");
    assert_eq!(volume.drive_type, DriveType::Fixed);
    assert!(!volume.is_remote());
    assert!(!volume.supports_usn, "there is no USN journal on macOS");
    assert!(volume.supports_hard_links);
    assert!(volume.supports_sparse);
    assert!(volume.supports_reparse_points);
    assert!(volume.supports_file_ids);
    assert!(
        volume.case_sensitive.is_some(),
        "APFS reports whether it is case sensitive"
    );
    assert!(volume.bytes_per_cluster > 0);
    assert_eq!(volume.native_feed, NativeFeed::MacosFsEvents);
    assert!(volume.is_native_local());
    assert_eq!(volume.source_kind(), eidos_domain::SourceKind::MacosLocal);
    assert!(!volume.volume_name.is_empty());
    assert!(Path::new(&volume.volume_root).is_absolute());
    assert_eq!(
        volume.volume_serial,
        listed_device(dir.path()),
        "the volume serial is the device both listers report"
    );
}

fn listed_device(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().dev()
}

#[test]
fn a_mount_point_is_recorded_but_not_traversed() {
    // `/Volumes` holds a mounted volume whenever one exists, and the data
    // volume is always mounted under `/System/Volumes`. Both listers see the
    // same children; only the native path knows which of them is a mount.
    let root = Path::new("/System/Volumes");
    let entries = match MacLister::new().list(root) {
        Ok(entries) => entries,
        Err(_) => return, // sandboxed or unreadable: nothing to assert.
    };
    let Some(data) = entries.iter().find(|e| e.name == "Data") else {
        return;
    };
    assert_eq!(data.kind, ObjectKind::Directory);
    assert!(
        data.attributes.is_reparse(),
        "a mounted volume is surfaced the way a Windows mount point is"
    );
    assert!(
        !data.is_traversable_dir(),
        "the walker must not descend into another volume"
    );
}

#[test]
fn firmlinked_system_paths_stay_traversable() {
    // `/Users` is a firmlink into the data volume. Refusing to traverse it
    // would hide every user file behind its only user-facing path.
    let entries = match MacLister::new().list(Path::new("/")) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let Some(users) = entries.iter().find(|e| e.name == "Users") else {
        return;
    };
    assert_eq!(users.kind, ObjectKind::Directory);
    assert!(
        users.is_traversable_dir(),
        "firmlinks are not mount points and must still be walked"
    );
}
