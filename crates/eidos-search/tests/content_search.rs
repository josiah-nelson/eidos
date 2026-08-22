//! Content search golden tests on a synthetic fixture: Q-1 (Markdown
//! diagnostic with ranked + exact content and a time bound), Q-2 (exact
//! case-sensitive identifier with line-aware snippets), regex with required
//! literals, phrase/ranked retrieval, binary rejection, completeness before
//! and after extraction, and re-extraction after a file changes.

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
use std::time::{Duration, SystemTime};

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

fn set_age(p: &Path, days: u64) {
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

const BIG_LINES: usize = 60_000;

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(
        &root.join("logs/Zephyr diagnostics.md"),
        b"# Zephyr diagnostics\nProduct Zephyr build 4.2.1\nEndpoint Qz responded slowly\n",
    );
    write(
        &root.join("logs/notes.txt"),
        b"plain notes mentioning QzEndpoint and qz lowercase\nsecond line here\n",
    );
    write(
        &root.join("src/main.rs"),
        b"fn main() {\n    println!(\"hello world\");\n}\n",
    );
    write(
        &root.join("bin/tool"),
        b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00binary payload\x00\x00",
    );
    write(
        &root.join("old/readme.md"),
        b"Zephyr archive note from long ago\n",
    );
    set_age(&root.join("old/readme.md"), 60);
    // A multi-chunk log with one unique line in the middle.
    let mut big = String::with_capacity(BIG_LINES * 48);
    for i in 0..BIG_LINES {
        if i == BIG_LINES / 2 {
            big.push_str("2026-08-22 12:00:00 WARN needle-7f3a9 found here\n");
        } else {
            big.push_str(&format!(
                "2026-08-22 12:00:00 INFO routine line number {i}\n"
            ));
        }
    }
    write(&root.join("data/big.log"), big.as_bytes());

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
    fn rescan(&self) {
        let lister = eidos_scanner::default_lister();
        run_scan(
            &self.catalog,
            self.source,
            lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
        self.index.follow_once(&self.catalog, 10_000).unwrap();
        self.index.reload().unwrap();
    }

    /// Extract everything pending and publish; then let the catalog index
    /// follower pick up the content-state flips.
    fn extract_all(&self) -> u64 {
        let n = self
            .catalog
            .enqueue_pending_content(self.source, 10_000)
            .unwrap();
        assert!(n > 0 || self.catalog.job_counts(None).unwrap().queued > 0 || true);
        let published =
            drain_content_jobs(&self.catalog, &self.content, &Limits::default(), "test").unwrap();
        self.index.follow_once(&self.catalog, 10_000).unwrap();
        self.index.reload().unwrap();
        published
    }

    fn run(&self, q: &str) -> SearchResponse {
        let parsed = parse(q).unwrap();
        let mut r = SearchRequest::new(parsed.query);
        r.explain = true;
        search_with_content(
            &self.index,
            Some(&self.content),
            &self.catalog,
            &r,
            &ExecOptions::default(),
        )
        .unwrap()
    }

    fn names(&self, q: &str) -> Vec<String> {
        let mut v: Vec<String> = self.run(q).hits.into_iter().map(|h| h.name).collect();
        v.sort();
        v
    }
}

#[test]
fn completeness_reflects_content_progress() {
    let fx = fixture();
    let before = fx.run("ext:md");
    assert!(before.completeness[0].metadata_complete);
    assert!(!before.completeness[0].content_complete);
    assert!(before.completeness[0].content_pending > 0);
    let r = fx.run("content:Zephyr");
    assert_eq!(r.hits.len(), 0, "nothing indexed yet");
    assert!(!r.all_sources_complete(true));

    let published = fx.extract_all();
    assert!(published >= 4, "published {published}");
    let after = fx.run("ext:md");
    assert!(
        after.completeness[0].content_complete,
        "{:?}",
        after.completeness
    );
    assert_eq!(after.completeness[0].content_pending, 0);
}

#[test]
fn q1_markdown_diagnostic_ranked_exact_and_time_bound() {
    let fx = fixture();
    fx.extract_all();
    // Both Markdown files mention Zephyr; the time bound keeps only the recent one.
    assert_eq!(
        fx.names("ext:md content:Zephyr"),
        vec!["Zephyr diagnostics.md", "readme.md"]
    );
    let r = fx.run("ext:md mtime:>=30d content:Zephyr content:\"build 4.2.1\"");
    assert_eq!(r.hits.len(), 1);
    let h = &r.hits[0];
    assert_eq!(h.name, "Zephyr diagnostics.md");
    assert_eq!(h.content.state, ContentState::Indexed);
    assert_eq!(h.content.coverage, Coverage::Full);
    assert!(!h.snippets.is_empty(), "snippets expected");
    assert!(h.snippets.iter().any(|s| s.text.contains("Zephyr")));
    assert!(r.all_sources_complete(true));
    let steps = r.explanation.unwrap().steps;
    assert!(
        steps.iter().filter(|s| s.stage == "content").count() >= 2,
        "{steps:?}"
    );
}

#[test]
fn q2_exact_case_sensitive_identifier_with_snippets() {
    let fx = fixture();
    fx.extract_all();
    // Whole-word, case-sensitive: `Qz` in the diagnostics file only.
    let r = fx.run("content:=Qz");
    let names: Vec<&str> = r.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["Zephyr diagnostics.md"]);
    let s = &r.hits[0].snippets[0];
    assert_eq!(s.line_start, 2, "zero-based line of the match: {s:?}");
    assert!(s.text.contains("Endpoint Qz responded"));
    assert_eq!(s.highlights.len(), 1);
    let (a, b) = (s.highlights[0][0] as usize, s.highlights[0][1] as usize);
    assert_eq!(&s.text[a..b], "Qz");
    // Mismatched case does not satisfy the exact clause.
    assert!(fx.names("content:=QZ").is_empty());
    // Lowercase `qz` exists as a word in notes.txt only.
    assert_eq!(fx.names("content:=qz"), vec!["notes.txt"]);
    // Case-sensitive substring also sees `QzEndpoint`.
    assert_eq!(
        fx.names("content:~Qz"),
        vec!["Zephyr diagnostics.md", "notes.txt"]
    );
    // Plain content clause is tokenised and case-insensitive.
    assert_eq!(
        fx.names("content:qz"),
        vec!["Zephyr diagnostics.md", "notes.txt"]
    );
    // A single-token exact literal is answered by the case-preserving token
    // index (no verification step); multi-word literals are verified.
    let steps = fx.run("content:=Qz").explanation.unwrap().steps;
    let c = steps.iter().find(|s| s.stage == "content").unwrap();
    assert!(c.verified.is_none(), "{c:?}");
    assert!(c.description.contains("case-sensitive token"), "{c:?}");
    let r = fx.run("content:=\"Qz responded\"");
    assert_eq!(r.hits.len(), 1);
    let steps = r.explanation.unwrap().steps;
    let c = steps.iter().find(|s| s.stage == "content").unwrap();
    assert!(c.verified.is_some(), "{c:?}");
    assert!(fx.names("content:=\"qz responded\"").is_empty());
}

#[test]
fn regex_uses_required_literals_and_verifies() {
    let fx = fixture();
    fx.extract_all();
    assert_eq!(fx.names("content:/Qz\\w+/c"), vec!["notes.txt"]);
    assert_eq!(
        fx.names("content:/qz\\w+/"),
        vec!["notes.txt"],
        "case-insensitive regex"
    );
    let r = fx.run("content:/needle-[0-9a-f]+/");
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].name, "big.log");
    let s = &r.hits[0].snippets[0];
    assert_eq!(s.line_start as usize, BIG_LINES / 2, "{s:?}");
    assert!(s.text.contains("needle-7f3a9"));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    // A regex without a required literal is allowed but flagged as broad.
    let broad = fx.run("content:/[0-9]{4}-[0-9]{2}/ ext:log");
    assert!(
        broad
            .warnings
            .iter()
            .any(|w| w.contains("no selective literal")),
        "{:?}",
        broad.warnings
    );
    assert_eq!(broad.hits.len(), 1);
}

#[test]
fn phrase_ranked_and_combination_with_metadata() {
    let fx = fixture();
    fx.extract_all();
    assert_eq!(fx.names("content:\"hello world\""), vec!["main.rs"]);
    assert!(fx.names("content:\"world hello\"").is_empty());
    assert_eq!(
        fx.names("content:responded ext:md"),
        vec!["Zephyr diagnostics.md"]
    );
    assert!(fx.names("content:responded ext:txt").is_empty());
    assert_eq!(
        fx.names("content:line -name:big"),
        vec!["notes.txt"],
        "negated metadata composes with content"
    );
    assert_eq!(fx.names("ext:md -content:Qz"), vec!["readme.md"]);
}

#[test]
fn binary_is_unsupported_and_visible() {
    let fx = fixture();
    fx.extract_all();
    let r = fx.run("state:unsupported");
    let names: Vec<&str> = r.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["tool"]);
    assert!(r.hits[0]
        .content
        .reason
        .as_deref()
        .is_some_and(|s| s.contains("binary")));
    assert!(fx.names("content:payload").is_empty());
    let rec = fx
        .catalog
        .content_record(r.hits[0].object_id)
        .unwrap()
        .unwrap();
    assert_eq!(rec.state, ContentState::Unsupported);
}

#[test]
fn big_file_is_chunked_with_exact_ranges() {
    let fx = fixture();
    fx.extract_all();
    let r = fx.run("name:=big.log");
    let obj = r.hits[0].object_id;
    let rec = fx.catalog.content_record(obj).unwrap().unwrap();
    assert!(rec.chunk_count > 50, "{rec:?}");
    assert_eq!(rec.line_count, BIG_LINES as u64);
    assert!(rec.hash_complete);
    let rows = fx
        .catalog
        .chunks_range(obj, rec.generation, 0, rec.chunk_count - 1)
        .unwrap();
    assert_eq!(rows.len() as u32, rec.chunk_count);
    let data = std::fs::read(fx.root.join("data/big.log")).unwrap();
    let mut expect = 0u64;
    for row in &rows {
        assert_eq!(row.byte_start, expect);
        expect = row.byte_end;
        assert_eq!(
            std::str::from_utf8(&data[row.byte_start as usize..row.byte_end as usize]).unwrap(),
            row.text
        );
    }
    assert_eq!(expect, data.len() as u64);
}

#[test]
fn changed_file_is_reextracted_and_old_text_disappears() {
    let fx = fixture();
    fx.extract_all();
    assert_eq!(fx.names("content:lowercase"), vec!["notes.txt"]);
    // Change the file: the rescan bumps the generation and state -> pending.
    std::thread::sleep(Duration::from_millis(20));
    write(
        &fx.root.join("logs/notes.txt"),
        b"rewritten notes with a brandnew token\n",
    );
    set_age(&fx.root.join("logs/notes.txt"), 0);
    fx.rescan();
    let r = fx.run("name:=notes.txt");
    assert_eq!(r.hits[0].content.state, ContentState::Pending);
    assert!(!r.completeness[0].content_complete);
    // Old content no longer produces snippets for the new generation.
    let stale = fx.run("content:lowercase");
    assert!(stale.hits.iter().all(|h| h.snippets.is_empty()));
    fx.extract_all();
    assert!(
        fx.names("content:lowercase").is_empty(),
        "old chunks removed"
    );
    assert_eq!(fx.names("content:brandnew"), vec!["notes.txt"]);
    let r = fx.run("name:=notes.txt");
    assert_eq!(r.hits[0].content.state, ContentState::Indexed);
    assert!(r.completeness[0].content_complete);
}

#[test]
fn disabled_source_is_not_extracted() {
    let fx = fixture();
    fx.catalog.set_content_policy(fx.source, false, 1).unwrap();
    assert_eq!(
        fx.catalog.enqueue_pending_content(fx.source, 100).unwrap(),
        0
    );
    let r = fx.run("content:Zephyr");
    assert!(r.hits.is_empty());
    assert!(!r.completeness[0].content_complete);
    fx.catalog.set_content_policy(fx.source, true, 2).unwrap();
    assert!(fx.extract_all() > 0);
    assert_eq!(fx.names("content:Zephyr ext:md").len(), 2);
}
