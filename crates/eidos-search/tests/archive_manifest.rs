//! ZIP manifests through the content pipeline (ADR-0010): container files
//! get a manifest and virtual members instead of text extraction, misnamed
//! text still indexes as text, corrupt directories fail cleanly, and
//! manifests survive a content reindex through `requeue_archives`.

#[cfg(windows)]
use eidos_archive::fixture::{build, Entry};
#[cfg(windows)]
use eidos_catalog::archive::{ArchiveMember, MemberQuery};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
#[cfg(windows)]
use eidos_content::Limits;
use eidos_domain::*;
#[cfg(windows)]
use eidos_query::parse;
#[cfg(windows)]
use eidos_search::exec::{search_with_content, ExecOptions};
#[cfg(windows)]
use eidos_search::pipeline::drain_content_jobs;
#[cfg(windows)]
use eidos_search::{CatalogIndex, ContentIndex};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<Catalog>,
    index: Arc<CatalogIndex>,
    content: Arc<ContentIndex>,
    source: SourceId,
}

fn write(p: &Path, body: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

#[cfg(windows)]
fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let good = build(
        &[
            Entry::file("readme.txt", b"hello"),
            Entry::dir("src/"),
            Entry::file("src/lib/mod.rs", b"fn x() {}"),
            Entry::file("192.0.2.7 (01)/asset.bin", &[0u8; 1000]),
            Entry::file("../escape.txt", b"x"),
        ],
        b"built for tests",
        false,
    );
    write(&root.join("pkg/tool.zip"), &good);
    write(
        &root.join("pkg/Plugin.VSIX"),
        &build(&[Entry::file("manifest.json", b"{}")], b"", true),
    );
    // Named like a container but plain text: must index as text.
    write(
        &root.join("pkg/notes.zip"),
        b"just some notes mentioning QzMarker here\n",
    );
    // Directory offset past the end record.
    let mut corrupt = build(&[Entry::file("a.txt", b"x")], b"", false);
    let n = corrupt.len();
    corrupt[n - 6..n - 2].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
    write(&root.join("pkg/broken.zip"), &corrupt);
    write(&root.join("docs/readme.md"), b"Zephyr notes\n");

    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("h", "windows").unwrap();
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
    let index = CatalogIndex::open(dir.path().join("index")).unwrap();
    index.sync_sources(&catalog).unwrap();
    let content = ContentIndex::open(dir.path().join("content")).unwrap();
    Fx {
        _dir: dir,
        root,
        catalog,
        index,
        content,
        source,
    }
}

#[cfg(windows)]
impl Fx {
    fn extract_all(&self) {
        self.catalog
            .enqueue_pending_content(self.source, 10_000)
            .unwrap();
        drain_content_jobs(&self.catalog, &self.content, &Limits::default(), "test").unwrap();
        self.index.follow_once(&self.catalog, 10_000).unwrap();
    }

    fn object(&self, rel: &str) -> ObjectId {
        self.catalog
            .resolve_relative(self.source, rel)
            .unwrap()
            .unwrap_or_else(|| panic!("{rel} not in catalog"))
    }

    fn run(&self, q: &str) -> SearchResponse {
        let parsed = parse(q).unwrap();
        let r = SearchRequest::new(parsed.query);
        search_with_content(
            &self.index,
            Some(&self.content),
            &self.catalog,
            &r,
            &ExecOptions::default(),
        )
        .unwrap()
    }
}

#[cfg(windows)]
#[test]
fn containers_get_manifests_and_members() {
    let fx = fixture();
    let _ = &fx.root;
    // Container jobs queue at manifest priority, behind text.
    fx.catalog
        .enqueue_pending_content(fx.source, 10_000)
        .unwrap();
    let zip = fx.object("pkg/tool.zip");
    let priority: i64 = fx
        .catalog
        .with_reader(|c| {
            Ok(c.query_row(
                "SELECT priority FROM jobs WHERE object_id = ?1 AND state = 'queued'",
                [zip.0],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(priority, Priority::ArchiveManifest as i64);
    fx.extract_all();

    let rec = fx.catalog.archive_record(zip).unwrap().expect("manifest");
    assert_eq!(rec.state, ContentState::Indexed);
    assert_eq!(rec.format, "zip");
    assert_eq!(rec.member_count, 5);
    assert_eq!(
        rec.dir_count, 3,
        "src (explicit), src/lib and the IP dir (implicit)"
    );
    assert_eq!(rec.implicit_dir_count, 2);
    assert_eq!(rec.declared_size, 5 + 9 + 1000 + 1);
    assert_eq!(rec.suspicious_count, 1);
    assert!(!rec.truncated && !rec.zip64);
    assert_eq!(rec.comment.as_deref(), Some("built for tests"));

    // Root listing: directories first, then files by name.
    let (root, total) = fx
        .catalog
        .archive_members(
            zip,
            &MemberQuery {
                parent: Some(String::new()),
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    let names: Vec<(&str, bool)> = root.iter().map(|m| (m.name.as_str(), m.is_dir)).collect();
    assert_eq!(
        names,
        vec![
            ("192.0.2.7 (01)", true),
            ("src", true),
            ("escape.txt", false),
            ("readme.txt", false)
        ]
    );
    assert_eq!(total, 4);
    assert!(root[2].flags & eidos_archive::zip::flag::TRAVERSAL != 0);
    let (under_src, total) = fx
        .catalog
        .archive_members(
            zip,
            &MemberQuery {
                prefix: Some("src/".into()),
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(under_src[0].path, "src/lib");
    assert!(under_src[0].implicit);
    assert_eq!(under_src[1].path, "src/lib/mod.rs");
    assert_eq!(under_src[1].size, 9);

    // The manifest is also a real virtual subtree, so ordinary catalog
    // traversal and the catalog index see the same rows as archive browsing.
    let virtual_file = fx.object("pkg/tool.zip/src/lib/mod.rs");
    let object = fx.catalog.get_object(virtual_file).unwrap().unwrap();
    assert_eq!(object.kind, ObjectKind::VirtualFile);
    assert_eq!(object.size, 9);
    let virtual_dir = fx.object("pkg/tool.zip/src/lib");
    assert_eq!(
        fx.catalog.get_object(virtual_dir).unwrap().unwrap().kind,
        ObjectKind::VirtualDirectory
    );
    let virtual_hits = fx.run("name:mod.rs");
    assert_eq!(virtual_hits.hits.len(), 1);
    assert_eq!(virtual_hits.hits[0].kind, ObjectKind::VirtualFile);
    assert!(virtual_hits.hits[0]
        .path
        .as_deref()
        .unwrap()
        .ends_with("tool.zip\\src\\lib\\mod.rs"));

    // The container's content state says what happened.
    let r = fx.run("name:tool.zip");
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].content.state, ContentState::Indexed);
    assert!(
        r.hits[0]
            .content
            .reason
            .as_deref()
            .unwrap_or("")
            .starts_with("zip manifest: 5 members"),
        "{:?}",
        r.hits[0].content
    );
    assert_eq!(
        fx.run("state:indexed ext:zip").hits.len(),
        2,
        "tool.zip and notes.zip"
    );

    // ZIP64 end records, upper-case extension.
    let vsix = fx
        .catalog
        .archive_record(fx.object("pkg/Plugin.VSIX"))
        .unwrap()
        .unwrap();
    assert!(vsix.zip64);
    assert_eq!(vsix.member_count, 1);

    // Misnamed text is still text; the verdict is kept so requeue skips it.
    let notes = fx.object("pkg/notes.zip");
    let marker = fx.catalog.archive_record(notes).unwrap().unwrap();
    assert_eq!(marker.state, ContentState::Unsupported);
    assert!(marker
        .reason
        .as_deref()
        .unwrap()
        .contains("processed as text"));
    assert_eq!(fx.run("content:=QzMarker").hits.len(), 1);

    // Corrupt directory: failed, with the reason on the record.
    let broken = fx.object("pkg/broken.zip");
    let rec = fx.catalog.archive_record(broken).unwrap().unwrap();
    assert_eq!(rec.state, ContentState::Failed);
    assert!(
        rec.error.as_deref().unwrap().contains("extends past"),
        "{rec:?}"
    );
    let r = fx.run("name:broken.zip");
    assert_eq!(r.hits[0].content.state, ContentState::Failed);

    let stats = fx.catalog.archive_stats(None).unwrap();
    assert_eq!((stats.archives, stats.members, stats.failed), (3, 6, 1));
    assert_eq!(stats.declared_size, 1017, "tool.zip 1015 + Plugin.VSIX 2");
}

#[cfg(windows)]
#[test]
fn reindex_and_requeue_rebuild_manifests() {
    let fx = fixture();
    fx.extract_all();
    let zip = fx.object("pkg/tool.zip");
    assert!(fx.catalog.archive_record(zip).unwrap().is_some());
    // Everything has a manifest: nothing to queue.
    assert_eq!(fx.catalog.requeue_archives(Some(fx.source)).unwrap(), 0);

    // A content reindex drops manifests with the chunks …
    fx.catalog.reset_content_for_reindex().unwrap();
    assert!(fx.catalog.archive_record(zip).unwrap().is_none());
    assert!(fx
        .catalog
        .resolve_relative(fx.source, "pkg/tool.zip/src/lib/mod.rs")
        .unwrap()
        .is_none());
    // … and requeue finds every container again (3 containers; notes.zip is
    // text but carries the extension, so it is queued too and falls back).
    assert_eq!(fx.catalog.requeue_archives(None).unwrap(), 4);
    drain_content_jobs(&fx.catalog, &fx.content, &Limits::default(), "test").unwrap();
    let rec = fx.catalog.archive_record(zip).unwrap().expect("rebuilt");
    assert_eq!(rec.member_count, 5);
    assert!(fx
        .catalog
        .resolve_relative(fx.source, "pkg/tool.zip/src/lib/mod.rs")
        .unwrap()
        .is_some());
    assert_eq!(fx.catalog.requeue_archives(None).unwrap(), 0);
}

#[cfg(windows)]
#[test]
fn requeue_backfills_an_existing_manifest_without_virtual_rows() {
    let fx = fixture();
    fx.extract_all();
    let zip = fx.object("pkg/tool.zip");
    assert!(fx.catalog.archive_record(zip).unwrap().is_some());

    // This is the shape of a catalog upgraded from migration 5: the manifest
    // is current, but its object/entry projection has never been built.
    fx.catalog
        .with_writer(|conn| {
            let now = UnixNanos::now().0;
            conn.execute(
                "UPDATE entries SET deleted_at = ?2 WHERE object_id IN (
                     SELECT object_id FROM objects WHERE archive_container_id = ?1
                 )",
                [zip.0, now],
            )?;
            conn.execute(
                "UPDATE objects SET deleted_at = ?2 WHERE archive_container_id = ?1",
                [zip.0, now],
            )?;
            Ok(())
        })
        .unwrap();

    assert_eq!(fx.catalog.requeue_archives(Some(fx.source)).unwrap(), 1);
    drain_content_jobs(&fx.catalog, &fx.content, &Limits::default(), "test").unwrap();
    assert!(fx
        .catalog
        .resolve_relative(fx.source, "pkg/tool.zip/src/lib/mod.rs")
        .unwrap()
        .is_some());
}

#[cfg(windows)]
#[test]
fn stale_archive_generation_is_not_published() {
    let fx = fixture();
    fx.extract_all();
    let zip = fx.object("pkg/tool.zip");
    let original = fx.catalog.archive_record(zip).unwrap().unwrap();
    let stale_content = fx.catalog.content_record(zip).unwrap().unwrap();

    write(
        &fx.root.join("pkg/tool.zip"),
        &build(&[Entry::file("new.bin", &[7u8; 8_192])], b"changed", false),
    );
    run_scan(
        &fx.catalog,
        fx.source,
        eidos_scanner::default_lister().as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    let current = fx.catalog.get_object(zip).unwrap().unwrap();
    assert_eq!(current.generation, original.generation + 1);
    assert_eq!(current.content_state, ContentState::Pending);
    assert!(fx
        .catalog
        .resolve_relative(fx.source, "pkg/tool.zip/src/lib/mod.rs")
        .unwrap()
        .is_none());
    fx.index.sync_sources(&fx.catalog).unwrap();
    assert!(fx.run("name:mod.rs").hits.is_empty());

    let stale_record = eidos_catalog::archive::ArchiveRecord {
        member_count: 999,
        reason: Some("stale publication must not land".into()),
        ..original.clone()
    };
    assert!(!fx
        .catalog
        .store_archive(&stale_record, &[], &stale_content, None)
        .unwrap());

    assert_eq!(fx.catalog.archive_record(zip).unwrap(), Some(original));
    let current = fx.catalog.get_object(zip).unwrap().unwrap();
    assert_eq!(current.generation, stale_record.generation + 1);
    assert_eq!(current.content_state, ContentState::Pending);
}

#[cfg(windows)]
#[test]
fn manifest_rows_persist_in_batches_and_published_retries_are_idempotent() {
    let fx = fixture();
    fx.extract_all();
    let zip = fx.object("pkg/tool.zip");
    let mut rec = fx.catalog.archive_record(zip).unwrap().unwrap();
    let mut content = fx.catalog.content_record(zip).unwrap().unwrap();
    write(
        &fx.root.join("pkg/tool.zip"),
        &build(&[Entry::file("new.bin", &[7u8; 8_192])], b"changed", false),
    );
    run_scan(
        &fx.catalog,
        fx.source,
        eidos_scanner::default_lister().as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    let generation = fx.catalog.get_object(zip).unwrap().unwrap().generation;
    rec.generation = generation;
    content.generation = generation;
    content.state = ContentState::Indexed;
    content.coverage = Coverage::Full;
    let mut members: Vec<ArchiveMember> = (0..2_500u32)
        .map(|ordinal| {
            let name = format!("member-{ordinal:04}.txt");
            ArchiveMember {
                ordinal,
                path: name.clone(),
                name: name.clone(),
                parent: String::new(),
                raw_name: name,
                is_dir: false,
                implicit: false,
                size: 1,
                compressed: 1,
                method: 0,
                crc32: ordinal,
                modified: None,
                encrypted: false,
                flags: 0,
            }
        })
        .collect();
    // ZIPs may contain duplicate paths. Ordinal keeps their identities
    // distinct even though both browse entries render the same path.
    members[1].path = members[0].path.clone();
    members[1].name = members[0].name.clone();
    members[1].raw_name = members[0].raw_name.clone();
    rec.member_count = members.len() as u64;
    rec.dir_count = 0;
    rec.implicit_dir_count = 0;
    rec.declared_size = members.len() as u64;
    rec.compressed_size = members.len() as u64;
    rec.claimed_entries = members.len() as u64;
    assert!(fx
        .catalog
        .store_archive(&rec, &members, &content, None)
        .unwrap());
    let (_, total) = fx
        .catalog
        .archive_members(
            zip,
            &MemberQuery {
                parent: Some(String::new()),
                limit: 5_000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(total, 2_500);
    let duplicate_entries: i64 = fx
        .catalog
        .with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM entries e JOIN objects o ON o.object_id = e.object_id
                 WHERE o.archive_container_id = ?1 AND e.name = 'member-0000.txt'
                   AND e.deleted_at IS NULL AND o.deleted_at IS NULL",
                [zip.0],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(duplicate_entries, 2);

    let mut retry = rec.clone();
    let short = &members[..3];
    retry.member_count = short.len() as u64;
    retry.declared_size = short.len() as u64;
    retry.compressed_size = short.len() as u64;
    retry.claimed_entries = short.len() as u64;
    assert!(fx
        .catalog
        .store_archive(&retry, short, &content, None)
        .unwrap());
    write(
        &fx.root.join("pkg/tool.zip"),
        &build(&[Entry::file("newer.bin", &[8u8; 16_384])], b"newer", false),
    );
    run_scan(
        &fx.catalog,
        fx.source,
        eidos_scanner::default_lister().as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fx.catalog.get_object(zip).unwrap().unwrap().generation,
        generation + 1
    );
    let (_, total) = fx
        .catalog
        .archive_members(
            zip,
            &MemberQuery {
                parent: Some(String::new()),
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(total, 2_500);
    assert_eq!(fx.catalog.archive_record(zip).unwrap(), Some(rec));
}

#[test]
fn source_requeue_processes_more_than_one_bounded_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    for n in 0..300 {
        write(&root.join(format!("archive-{n:03}.zip")), b"not a zip");
    }
    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("h", "windows").unwrap();
    let source = catalog
        .add_source(&NewSource {
            host_id: host,
            name: "many".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
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

    assert_eq!(catalog.requeue_archives(Some(source)).unwrap(), 300);
    assert_eq!(catalog.requeue_archives(Some(source)).unwrap(), 0);
    let jobs: i64 = catalog
        .with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM jobs WHERE source_id = ?1 AND state = 'queued'",
                [source.0],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(jobs, 300);
}

#[test]
fn mixed_extension_hard_link_priority_matches_rendered_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let rendered = root.join("a/plain.txt");
    let archive_alias = root.join("z/alias.zip");
    write(&rendered, b"ordinary text");
    std::fs::create_dir_all(archive_alias.parent().unwrap()).unwrap();
    std::fs::hard_link(&rendered, &archive_alias).unwrap();

    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("h", "windows").unwrap();
    let source = catalog
        .add_source(&NewSource {
            host_id: host,
            name: "links".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
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
        .resolve_relative(source, "a/plain.txt")
        .unwrap()
        .unwrap();
    assert_eq!(
        catalog
            .resolve_relative(source, "z/alias.zip")
            .unwrap()
            .unwrap(),
        object
    );
    let target = catalog.content_target(object).unwrap().unwrap();
    assert!(target.path.ends_with("plain.txt") || target.path.ends_with("alias.zip"));
    let rendered_is_archive = eidos_domain::archive::archive_format(&target.path).is_some();

    assert_eq!(
        catalog.requeue_archives(Some(source)).unwrap(),
        u64::from(rendered_is_archive)
    );
    assert_eq!(
        catalog.enqueue_pending_content(source, 100).unwrap(),
        u64::from(!rendered_is_archive)
    );
    let priority: i64 = catalog
        .with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT priority FROM jobs WHERE object_id = ?1",
                [object.0],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    let expected = if rendered_is_archive {
        Priority::ArchiveManifest
    } else {
        Priority::SmallText
    };
    assert_eq!(priority, expected as i64);
}

#[cfg(windows)]
#[test]
fn projection_rows_agree_between_rebuild_and_incremental_for_virtual_members() {
    let fx = fixture();
    fx.extract_all();
    let mut rebuild = Vec::new();
    fx.catalog
        .for_each_projection_row(fx.source, |row| {
            rebuild.push(row);
            Ok(())
        })
        .unwrap();
    assert!(
        rebuild.iter().any(|r| r.kind.is_virtual()),
        "fixture has no virtual entries"
    );

    // A container is a path node: members render underneath it and carry it
    // in their ancestor chain.
    let zip = fx.object("pkg/tool.zip");
    let zip_path = fx.catalog.render_path(zip).unwrap().unwrap();
    let member = rebuild
        .iter()
        .find(|r| r.name == "mod.rs")
        .expect("member row");
    assert_eq!(member.path, format!(r"{zip_path}\src\lib\mod.rs"));
    assert!(member.ancestors.contains(&zip), "{:?}", member.ancestors);

    // Field for field, a rebuilt row is the row the follower's incremental
    // path produces for the same object — old implementation and new.
    let mut objects: Vec<ObjectId> = rebuild.iter().map(|r| r.object_id).collect();
    objects.sort_unstable_by_key(|o| o.0);
    objects.dedup();
    for object in objects {
        let reference = fx
            .catalog
            .reference_projection_rows_for_object(object)
            .unwrap();
        assert_eq!(
            fx.catalog.projection_rows_for_object(object).unwrap(),
            reference,
            "batched per-object rows differ for {object:?}"
        );
        let rebuilt: Vec<_> = rebuild
            .iter()
            .filter(|r| r.object_id == object)
            .cloned()
            .collect();
        assert_eq!(rebuilt, reference, "rebuild differs for {object:?}");
    }
}
