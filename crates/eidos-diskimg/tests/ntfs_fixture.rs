//! End-to-end inventory of a real NTFS-in-VHDX image.
//!
//! The image under `tests/fixtures/` is built by
//! `scripts/make-diskimg-fixture.ps1`, which needs an elevated shell and the
//! Hyper-V PowerShell module. When it is absent these tests print a note and
//! pass, the same way the USN journal tests behave without elevation — the
//! container, partition, and corruption paths are covered by synthetic bytes
//! in the crate's own unit tests and do not depend on this file.

use eidos_diskimg::{
    flag, inventory_all, DiskImageError, DiskImageLimits, ImageFormat, Member, Outcome, PayloadKind,
};
use std::io::Cursor;
use std::sync::OnceLock;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/ntfs-gpt-dynamic.vhdx.zst"
);

/// The decompressed image, or `None` when the fixture was never built.
fn image() -> Option<&'static [u8]> {
    static IMAGE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            let packed = std::fs::read(FIXTURE).ok()?;
            match zstd::decode_all(Cursor::new(packed)) {
                Ok(raw) => Some(raw),
                Err(e) => panic!("the disk-image fixture is present but unreadable: {e}"),
            }
        })
        .as_deref()
}

/// Fetch the fixture, or print why the test is being skipped and bail out.
macro_rules! image_or_skip {
    () => {
        match image() {
            Some(image) => image,
            None => {
                eprintln!("skipping disk-image test: run scripts/make-diskimg-fixture.ps1 first");
                return;
            }
        }
    };
}

fn inventory(limits: &DiskImageLimits) -> (eidos_diskimg::ImageReport, Vec<Member>) {
    let image = image().expect("fixture");
    inventory_all(Cursor::new(image), image.len() as u64, limits).expect("inventory")
}

fn find<'a>(members: &'a [Member], path: &str) -> &'a Member {
    members
        .iter()
        .find(|m| m.path == path)
        .unwrap_or_else(|| panic!("no member at {path}"))
}

#[test]
fn the_image_reports_its_container_and_partitions() {
    image_or_skip!();
    let (report, _) = inventory(&DiskImageLimits::default());
    assert_eq!(report.format, ImageFormat::Vhdx);
    assert_eq!(report.image.payload, PayloadKind::Dynamic);
    assert_eq!(report.outcome, Outcome::Complete);
    assert_eq!(report.image.parent, None);
    assert!(
        report.failed_volumes.is_empty(),
        "{:?}",
        report.failed_volumes
    );
    assert_eq!(report.scheme, Some(eidos_diskimg::PartitionScheme::Gpt));
    assert_eq!(report.image.virtual_size, 64 * 1024 * 1024);
    assert!(report.image.block_size >= 1024 * 1024);
    assert!(report.image.present_blocks > 0);
    assert!(!report.partitions.is_empty());
    let data: Vec<_> = report.partitions.iter().filter(|p| p.ntfs).collect();
    assert_eq!(data.len(), 2);
    assert!(data.iter().all(|partition| {
        partition.type_label == "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7"
            && partition.start > 0
            && partition.start + partition.length <= report.image.virtual_size
    }));
    assert_eq!(report.volumes.len(), 2);
    assert_eq!(report.volumes[0].label, "eidosfix");
    assert_eq!(report.volumes[1].label, "eidosaux");

    for volume in &report.volumes {
        assert_eq!(volume.cluster_size, 4096);
        assert_eq!(volume.outcome, Outcome::Complete);
        assert!(!volume.dirty);
        assert_eq!(volume.inexact_allocation_count, 0);
        assert_eq!(volume.skipped_deep, 0);
        assert!(volume.mft_records > 0);
        // Every NTFS volume carries at least $MFT, $LogFile, $Volume,
        // $Bitmap, and the $Extend subtree; none may reach the catalog.
        assert!(volume.metafile_count >= 10, "{}", volume.metafile_count);
    }
    assert_eq!(
        report
            .volumes
            .iter()
            .map(|volume| volume.member_count)
            .sum::<u64>(),
        report.member_count
    );
}

#[test]
fn the_written_tree_comes_back_intact() {
    image_or_skip!();
    let (report, members) = inventory(&DiskImageLimits::default());

    let brindle = find(&members, "brindle.bin");
    assert!(!brindle.is_dir);
    assert_eq!(brindle.size, 4096);
    assert_eq!(brindle.parent, "");
    assert_eq!(brindle.name, "brindle.bin");
    assert_eq!(brindle.hard_links, 2);
    assert!(brindle.modified.is_some() && brindle.created.is_some());
    // 4 KiB of data in 4 KiB clusters occupies exactly one cluster.
    assert_eq!(brindle.allocated, 4096);
    assert!(brindle.allocation_exact);
    let linked = find(&members, "corpus/brindle-link.bin");
    assert_eq!(linked.record, brindle.record);
    assert_eq!(linked.size, brindle.size);
    assert_eq!(linked.hard_links, 2);

    let ledger = find(&members, "corpus/alcove/ledger.txt");
    assert_eq!(ledger.size, 37);
    assert_eq!(ledger.parent, "corpus/alcove");
    // Small files live inside the MFT record itself, occupying no clusters.
    assert_eq!(ledger.allocated, 0);
    assert!(ledger.allocation_exact);

    let quillon = find(&members, "corpus/alcove/quillon.dat");
    assert_eq!(quillon.size, 100_000);
    assert!(quillon.allocated >= quillon.size && quillon.allocated % 4096 == 0);
    assert!(quillon.allocation_exact);

    for directory in ["corpus", "corpus/alcove", "corpus/zephyr", "hollow"] {
        let member = find(&members, directory);
        assert!(member.is_dir, "{directory}");
        assert_eq!(member.size, 0);
    }

    // No `$`-prefixed metafile and no root entry may be emitted.
    assert!(!members.iter().any(|m| m.path.starts_with('$')));
    assert!(!members.iter().any(|m| m.path.is_empty()));
    let auxiliary = find(&members, "auxiliary/marker.txt");
    assert_eq!(auxiliary.volume, 1);
    assert_eq!(auxiliary.size, 23);
    assert_eq!(members.len() as u64, report.member_count);

    for (index, volume) in report.volumes.iter().enumerate() {
        let volume_members: Vec<_> = members
            .iter()
            .filter(|member| member.volume == index as u32)
            .collect();
        let files = volume_members
            .iter()
            .filter(|member| !member.is_dir)
            .count() as u64;
        let declared: u64 = volume_members
            .iter()
            .filter(|member| !member.is_dir)
            .map(|member| member.size)
            .sum();
        assert_eq!(volume.declared_size, declared);
        assert_eq!(volume.dir_count + files, volume.member_count);
    }
}

#[test]
fn unicode_names_and_deep_paths_survive() {
    image_or_skip!();
    let (_, members) = inventory(&DiskImageLimits::default());

    let unicode = find(&members, "corpus/zephyr/grünwald-πλούτος.txt");
    assert_eq!(unicode.name, "grünwald-πλούτος.txt");
    assert_eq!(unicode.size, 11);
    // A name that round-trips UTF-16 to UTF-8 is not suspicious.
    assert_eq!(unicode.flags, 0);

    let deep = members
        .iter()
        .find(|m| m.name == "sable.txt")
        .expect("no deep member");
    let segments: Vec<&str> = deep.path.split('/').collect();
    assert_eq!(segments.len(), 25, "{}", deep.path);
    assert_eq!(segments[0], "vellum");
    assert_eq!(segments[1], "tier01");
    assert_eq!(deep.size, 64);
    assert_eq!(deep.flags, 0);
    assert_eq!(
        members
            .iter()
            .filter(|m| m.flags & flag::ORPHAN != 0)
            .count(),
        0
    );
}

#[test]
fn budgets_truncate_the_inventory_instead_of_failing_it() {
    image_or_skip!();

    // A path-depth budget below the deepest member drops that member and
    // marks the inventory partial; everything shallower still arrives.
    let shallow = DiskImageLimits {
        max_path_depth: 8,
        ..DiskImageLimits::default()
    };
    let (report, members) = inventory(&shallow);
    assert_eq!(report.outcome, Outcome::Partial);
    assert!(report.volumes[0].skipped_deep > 0);
    assert!(report
        .partial_reasons
        .iter()
        .any(|reason| reason.contains("path budget")));
    assert!(!members.iter().any(|m| m.name == "sable.txt"));
    find(&members, "brindle.bin");

    // A member budget stops the MFT scan early.
    let few = DiskImageLimits {
        max_members: 4,
        ..DiskImageLimits::default()
    };
    let (report, members) = inventory(&few);
    assert_eq!(report.outcome, Outcome::Partial);
    assert!(!members.is_empty());
    assert!(members.len() <= 4);
    assert!(report
        .partial_reasons
        .iter()
        .any(|reason| reason.contains("member limit")));

    // So does an MFT-record budget.
    let short = DiskImageLimits {
        max_mft_records: 8,
        ..DiskImageLimits::default()
    };
    let (report, _) = inventory(&short);
    assert_eq!(report.outcome, Outcome::Partial);
    assert!(report.volumes[0].scanned_records <= 8);
    assert!(report
        .partial_reasons
        .iter()
        .any(|reason| reason.contains("MFT record limit")));

    // The member allowance belongs to the image, not independently to each
    // NTFS volume. Choose a limit one below the known-complete fixture.
    let (complete, _) = inventory(&DiskImageLimits::default());
    let aggregate_limit = complete.member_count - 1;
    let aggregate = DiskImageLimits {
        max_members: aggregate_limit,
        ..DiskImageLimits::default()
    };
    let (report, members) = inventory(&aggregate);
    assert_eq!(report.outcome, Outcome::Partial);
    assert_eq!(report.volumes.len(), 2);
    assert!(members.len() as u64 <= aggregate_limit);
    assert_eq!(report.member_count, members.len() as u64);
    assert!(report
        .partial_reasons
        .iter()
        .any(|reason| reason.contains("aggregate member limit")));
}

#[test]
fn truncated_and_damaged_images_never_panic() {
    let image = image_or_skip!();
    let run = |bytes: &[u8]| {
        inventory_all(
            Cursor::new(bytes.to_vec()),
            bytes.len() as u64,
            &DiskImageLimits::default(),
        )
        .map(|(_, members)| members.into_iter().map(|m| m.path).collect::<Vec<_>>())
    };
    let (_, whole) = inventory(&DiskImageLimits::default());
    let known: std::collections::HashSet<&str> = whole.iter().map(|m| m.path.as_str()).collect();

    // Cut short of the regions the region table promises.
    let stub = &image[..2 * 1024 * 1024];
    assert!(
        matches!(run(stub), Err(DiskImageError::Corrupt(_))),
        "{:?}",
        run(stub)
    );

    // Arbitrary truncations and single-byte damage across the container
    // area must fail cleanly or return a subset of the real tree — never a
    // panic, and never a member the intact image does not have.
    for sixty_fourths in [1usize, 5, 11, 23, 47, 61] {
        if let Ok(paths) = run(&image[..image.len() * sixty_fourths / 64]) {
            assert!(
                paths.len() <= whole.len(),
                "truncated to {sixty_fourths}/64"
            );
            for path in paths {
                assert!(known.contains(path.as_str()), "invented {path}");
            }
        }
    }
    for offset in (0..4 * 1024 * 1024).step_by(512 * 1024) {
        let mut damaged = image.to_vec();
        damaged[offset] ^= 0xFF;
        let _ = run(&damaged);
    }
}
