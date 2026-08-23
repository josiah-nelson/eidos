//! ZIP manifests through the content pipeline (ADR-0010): container files
//! get a manifest and virtual members instead of text extraction, misnamed
//! text still indexes as text, corrupt directories fail cleanly, and
//! manifests survive a content reindex through `requeue_archives`.

use eidos_archive::fixture::{build, Entry};
use eidos_catalog::archive::MemberQuery;
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_content::Limits;
use eidos_domain::*;
use eidos_query::parse;
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_search::pipeline::drain_content_jobs;
use eidos_search::{CatalogIndex, ContentIndex};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let good = build(
        &[
            Entry::file("readme.txt", b"hello"),
            Entry::dir("src/"),
            Entry::file("src/lib/mod.rs", b"fn x() {}"),
            Entry::file("10.0.0.7 (01)/asset.bin", &[0u8; 1000]),
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
    index.reload().unwrap();
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

impl Fx {
    fn extract_all(&self) {
        self.catalog
            .enqueue_pending_content(self.source, 10_000)
            .unwrap();
        drain_content_jobs(&self.catalog, &self.content, &Limits::default(), "test").unwrap();
        self.index.follow_once(&self.catalog, 10_000).unwrap();
        self.index.reload().unwrap();
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
            ("10.0.0.7 (01)", true),
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
    // … and requeue finds every container again (3 containers; notes.zip is
    // text but carries the extension, so it is queued too and falls back).
    assert_eq!(fx.catalog.requeue_archives(None).unwrap(), 4);
    drain_content_jobs(&fx.catalog, &fx.content, &Limits::default(), "test").unwrap();
    let rec = fx.catalog.archive_record(zip).unwrap().expect("rebuilt");
    assert_eq!(rec.member_count, 5);
    assert_eq!(fx.catalog.requeue_archives(None).unwrap(), 0);
}
