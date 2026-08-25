//! Common temporary-filesystem contract for scanner adapters.
//!
//! Every adapter this platform can use for a local volume runs the same
//! fixtures, so a native fast path cannot quietly diverge from the portable
//! reference. The fixtures are synthetic and run without a native change
//! journal; a second scan represents restart reconciliation from durable
//! prior state.

use eidos_domain::{NativeIdentity, ObjectKind};
use eidos_scanner::std_lister::StdLister;
use eidos_scanner::{walk, DirectoryLister, WalkOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Fixtures normally live in the platform temporary directory. Setting
/// `EIDOS_TEST_VOLUME` to a writable path runs the same contract on another
/// volume, which is how case-sensitive APFS, exFAT, and SMB behaviour is
/// exercised without assuming any of them exist on a given host.
fn fixture_root() -> tempfile::TempDir {
    match std::env::var_os("EIDOS_TEST_VOLUME") {
        Some(volume) => tempfile::Builder::new()
            .prefix("eidos-contract-")
            .tempdir_in(volume)
            .expect("EIDOS_TEST_VOLUME must be a writable directory"),
        None => tempfile::tempdir().unwrap(),
    }
}

/// The portable lister plus whatever native adapter this build has.
fn listers() -> Vec<(&'static str, Box<dyn DirectoryLister>)> {
    let mut listers: Vec<(&'static str, Box<dyn DirectoryLister>)> =
        vec![("std", Box::new(StdLister))];
    #[cfg(target_os = "macos")]
    listers.push((
        "macos",
        Box::new(eidos_scanner::mac::MacLister::new()) as Box<dyn DirectoryLister>,
    ));
    #[cfg(windows)]
    listers.push((
        "windows",
        Box::new(eidos_scanner::win::WinLister::new()) as Box<dyn DirectoryLister>,
    ));
    listers
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryView {
    kind: ObjectKind,
    size: u64,
    native_id: Option<NativeIdentity>,
}

fn snapshot(root: &Path, lister: &dyn DirectoryLister) -> (BTreeMap<PathBuf, EntryView>, usize) {
    let mut entries = BTreeMap::new();
    let mut errors = 0;
    walk(
        root,
        lister,
        &WalkOptions {
            threads: 2,
            ..Default::default()
        },
        |event| match event.result {
            Ok(children) => {
                for child in children {
                    let absolute = event.path.join(&child.name);
                    let relative = absolute.strip_prefix(root).unwrap().to_path_buf();
                    entries.insert(
                        relative,
                        EntryView {
                            kind: child.kind,
                            size: child.size,
                            native_id: child.native_id,
                        },
                    );
                }
            }
            Err(_) => errors += 1,
        },
    );
    (entries, errors)
}

#[test]
fn create_update_delete_and_restart_reconciliation() {
    for (adapter, lister) in listers() {
        let temporary = fixture_root();
        let root = temporary.path();
        std::fs::create_dir(root.join("bramble")).unwrap();
        std::fs::write(root.join("bramble/alpha.txt"), b"one").unwrap();
        std::fs::write(root.join("cedar.txt"), b"remove").unwrap();
        let (before, errors) = snapshot(root, lister.as_ref());
        assert_eq!(errors, 0, "{adapter}");

        std::fs::write(root.join("bramble/alpha.txt"), b"updated-value").unwrap();
        std::fs::write(root.join("bramble/delta.txt"), b"created").unwrap();
        std::fs::remove_file(root.join("cedar.txt")).unwrap();

        // A fresh walk models a restarted scanner reconciling from the
        // previously durable snapshot.
        let (after, errors) = snapshot(root, lister.as_ref());
        assert_eq!(errors, 0, "{adapter}");
        let before_names: BTreeSet<_> = before.keys().cloned().collect();
        let after_names: BTreeSet<_> = after.keys().cloned().collect();
        assert_eq!(
            after_names
                .difference(&before_names)
                .cloned()
                .collect::<Vec<_>>(),
            vec![PathBuf::from("bramble/delta.txt")],
            "{adapter}"
        );
        assert_eq!(
            before_names
                .difference(&after_names)
                .cloned()
                .collect::<Vec<_>>(),
            vec![PathBuf::from("cedar.txt")],
            "{adapter}"
        );
        assert_ne!(
            before[Path::new("bramble/alpha.txt")].size,
            after[Path::new("bramble/alpha.txt")].size,
            "{adapter}"
        );
    }
}

#[test]
fn rename_directory_move_hard_link_and_symlink() {
    for (adapter, lister) in listers() {
        let temporary = fixture_root();
        let root = temporary.path();
        std::fs::create_dir_all(root.join("elm/inner")).unwrap();
        std::fs::create_dir(root.join("fir")).unwrap();
        std::fs::write(root.join("elm/inner/item.bin"), b"payload").unwrap();
        std::fs::rename(
            root.join("elm/inner/item.bin"),
            root.join("elm/inner/renamed.bin"),
        )
        .unwrap();
        std::fs::rename(root.join("elm/inner"), root.join("fir/moved")).unwrap();
        std::fs::hard_link(
            root.join("fir/moved/renamed.bin"),
            root.join("hard-link.bin"),
        )
        .unwrap();
        let symlink_created = make_symlink(
            Path::new("fir/moved/renamed.bin"),
            &root.join("symbolic-link.bin"),
        );

        let (entries, errors) = snapshot(root, lister.as_ref());
        assert_eq!(errors, 0, "{adapter}");
        assert!(!entries.contains_key(Path::new("elm/inner")), "{adapter}");
        assert!(
            entries.contains_key(Path::new("fir/moved/renamed.bin")),
            "{adapter}"
        );
        assert_eq!(
            entries[Path::new("hard-link.bin")].native_id,
            entries[Path::new("fir/moved/renamed.bin")].native_id,
            "{adapter}: hard links share one identity"
        );
        if symlink_created {
            assert_eq!(
                entries[Path::new("symbolic-link.bin")].kind,
                ObjectKind::Reparse,
                "{adapter}"
            );
        }
    }
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> bool {
    if let Err(error) = std::os::windows::fs::symlink_file(target, link) {
        eprintln!("skipping symlink fixture: platform permission unavailable ({error})");
        std::fs::write(link, b"symlink-unavailable").unwrap();
        false
    } else {
        true
    }
}

#[test]
fn case_behavior_and_unicode_nfd_names_are_observed() {
    for (adapter, lister) in listers() {
        let temporary = fixture_root();
        let root = temporary.path();
        std::fs::write(root.join("MapleCase.txt"), b"upper").unwrap();
        let case_insensitive = root.join("maplecase.txt").exists();
        if !case_insensitive {
            std::fs::write(root.join("maplecase.txt"), b"lower").unwrap();
        }
        // Written in NFD; HFS+ normalises names and APFS does not, so the
        // adapter must report whatever the volume stored rather than a form
        // of its own choosing.
        let nfd_name = "cafe\u{301}-juniper.txt";
        std::fs::write(root.join(nfd_name), b"unicode").unwrap();

        let names: BTreeSet<_> = lister
            .list(root)
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert!(
            names.iter().any(|name| name.contains("juniper.txt")),
            "{adapter}"
        );
        assert!(
            names.contains(nfd_name) || names.contains("caf\u{e9}-juniper.txt"),
            "{adapter}: the name must round-trip in one of the volume's forms"
        );
        if case_insensitive {
            assert_eq!(
                names
                    .iter()
                    .filter(|name| name.eq_ignore_ascii_case("MapleCase.txt"))
                    .count(),
                1,
                "{adapter}"
            );
        } else {
            assert!(names.contains("MapleCase.txt"), "{adapter}");
            assert!(names.contains("maplecase.txt"), "{adapter}");
        }
    }
}

#[test]
#[cfg(unix)]
fn permission_failure_is_an_error_not_a_traversal() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping permission fixture: root bypasses directory mode checks");
        return;
    }
    for (adapter, lister) in listers() {
        let temporary = fixture_root();
        let denied = temporary.path().join("locked-grove");
        std::fs::create_dir(&denied).unwrap();
        std::fs::write(denied.join("hidden.txt"), b"unreadable").unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (_, errors) = snapshot(temporary.path(), lister.as_ref());
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(errors, 1, "{adapter}: an unreadable directory is one error");
    }
}
