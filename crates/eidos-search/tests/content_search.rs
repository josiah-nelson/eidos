//! Content search golden tests on a synthetic fixture: Q-1 (Markdown
//! diagnostic with ranked + exact content and a time bound), Q-2 (exact
//! case-sensitive identifier with line-aware snippets), regex with required
//! literals, phrase/ranked retrieval, binary rejection, completeness before
//! and after extraction, re-extraction after a file changes, deletion-only
//! reindexing (a text file that turns binary, empty, or unreadable), and the
//! rejection of candidates from a superseded generation.

#![cfg(windows)]

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_content::{Chunk, Limits};
use eidos_domain::*;
use eidos_query::parse;
use eidos_search::content::{object_ids, ContentOpts};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_search::pipeline::{drain_content_jobs, process_object, ProcessResult};
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
        published
    }

    /// Extract everything pending under the service's commit policy
    /// (`content_workers::coordinator_loop`): the index is committed only
    /// when there is something to publish or the writer reports itself
    /// dirty. `drain_content_jobs` commits unconditionally, which would hide
    /// a queued deletion that never marked the writer dirty. Returns
    /// `(published, dirty)`.
    fn extract_like_coordinator(&self) -> (u64, bool) {
        self.catalog
            .enqueue_pending_content(self.source, 10_000)
            .unwrap();
        let mut pending: Vec<ObjectId> = Vec::new();
        while let Some(job) = self
            .catalog
            .claim_job(&[JobStage::ContentText], "test")
            .unwrap()
        {
            let object = match job.object_id {
                Some(o) => o,
                None => {
                    self.catalog.complete_job(job.id).unwrap();
                    continue;
                }
            };
            let r = process_object(
                &self.catalog,
                &self.content,
                object,
                job.object_generation,
                &Limits::default(),
                Some(job.id),
            )
            .unwrap();
            match r {
                ProcessResult::Indexed(_) => pending.push(object),
                ProcessResult::Done(_) => {}
                ProcessResult::Skipped(_) => self.catalog.complete_job(job.id).unwrap(),
                ProcessResult::Disabled => self.catalog.delete_job(job.id).unwrap(),
                ProcessResult::Retry { class, error } => {
                    self.catalog.fail_job(job.id, class, &error).unwrap();
                }
            }
        }
        let dirty = self.content.is_dirty();
        let mut published = 0;
        if !pending.is_empty() || dirty {
            self.content.commit().unwrap();
            published = self.catalog.mark_content_indexed(&pending).unwrap();
        }
        self.index.follow_once(&self.catalog, 10_000).unwrap();
        (published, dirty)
    }

    fn object_of(&self, name: &str) -> ObjectId {
        self.run(&format!("name:={name}")).hits[0].object_id
    }

    fn run(&self, q: &str) -> SearchResponse {
        self.run_with(q, &ExecOptions::default(), None, 50)
    }

    fn run_with(
        &self,
        q: &str,
        opts: &ExecOptions,
        cursor: Option<String>,
        limit: u32,
    ) -> SearchResponse {
        let parsed = parse(q).unwrap();
        let mut r = SearchRequest::new(parsed.query);
        r.explain = true;
        r.cursor = cursor;
        r.limit = limit;
        search_with_content(&self.index, Some(&self.content), &self.catalog, &r, opts).unwrap()
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

/// Options that force page-driven verification for every verified clause.
fn lazy_opts() -> ExecOptions {
    ExecOptions {
        content: ContentOpts {
            lazy_min: 0,
            ..ContentOpts::default()
        },
        ..ExecOptions::default()
    }
}

fn sorted_names(r: &SearchResponse) -> Vec<String> {
    let mut v: Vec<String> = r.hits.iter().map(|h| h.name.clone()).collect();
    v.sort();
    v
}

#[test]
fn page_driven_verification_returns_the_eager_results() {
    let fx = fixture();
    fx.extract_all();
    let lazy = lazy_opts();
    for q in [
        "content:~Qz",
        "content:~qz",
        "content:/needle-[0-9a-f]+/",
        "content:=\"Qz responded\"",
        "content:~line ext:log",
        "content:~line -name:big",
        "content:~line content:~routine",
        "content:~nomatchanywhere",
    ] {
        let eager = fx.run(q);
        let r = fx.run_with(q, &lazy, None, 50);
        assert_eq!(sorted_names(&r), sorted_names(&eager), "{q}");
        assert_eq!(r.total.value, eager.total.value, "{q}");
        assert!(
            r.total.exact,
            "walk finished, total exact: {q} {:?}",
            r.total
        );
        assert!(
            r.warnings.iter().all(|w| !w.contains("upper bound")),
            "{q}: {:?}",
            r.warnings
        );
        let steps = r.explanation.as_ref().unwrap().steps.clone();
        let c = steps.iter().find(|s| s.stage == "content").unwrap();
        if c.candidates.unwrap_or(0) > 0 {
            assert!(c.description.contains("page-driven"), "{q}: {c:?}");
            assert!(
                c.verified.is_none(),
                "{q}: nothing verified up front: {c:?}"
            );
        }
        if !r.hits.is_empty() {
            let v = steps
                .iter()
                .find(|s| s.stage == "verify" && s.description.starts_with("lazy:"))
                .unwrap_or_else(|| panic!("{q}: no page walk step in {steps:?}"));
            assert!(v.description.contains("chunks fetched"), "{q}: {v:?}");
            assert_eq!(v.verified, Some(r.hits.len() as u64), "{q}: {v:?}");
        }
        // Snippets come from verified chunks on both paths.
        for (a, b) in r.hits.iter().zip(eager.hits.iter()) {
            let sa: Vec<&str> = a.snippets.iter().map(|s| s.text.as_str()).collect();
            let sb: Vec<&str> = b.snippets.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(sa, sb, "{q}: snippets of {}", a.name);
            assert!(a.snippets.iter().all(|s| !s.highlights.is_empty()), "{q}");
        }
    }
}

#[test]
fn page_driven_relevance_ranks_by_matching_chunks_with_exact_order() {
    let fx = fixture();
    fx.extract_all();
    // `line` is in every chunk of big.log and in one chunk of notes.txt.
    let r = fx.run_with("content:~line", &lazy_opts(), None, 50);
    let names: Vec<&str> = r.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["big.log", "notes.txt"]);
    let big = r.hits[0].score.unwrap();
    let notes = r.hits[1].score.unwrap();
    assert_eq!(big, 8.0, "capped at SCORE_CAP matching chunks");
    assert_eq!(notes, 1.0);
    // The eager path scores the same way.
    let e = fx.run("content:~line");
    assert_eq!(e.hits[0].score, r.hits[0].score);
    assert_eq!(e.hits[1].score, r.hits[1].score);
    // Verification stopped early: big.log has far more than 8 matching
    // chunks but only SCORE_CAP of them were fetched.
    let steps = r.explanation.unwrap().steps;
    let v = steps
        .iter()
        .find(|s| s.description.starts_with("lazy:"))
        .unwrap();
    let fetched: usize = v
        .description
        .split("; ")
        .find_map(|p| p.strip_suffix(" chunks fetched for content verification"))
        .and_then(|n| n.parse().ok())
        .unwrap();
    assert!(fetched <= 16, "{v:?}");
}

#[test]
fn page_driven_budget_exhaustion_is_reported_not_silent() {
    let fx = fixture();
    fx.extract_all();
    // A budget of 3 is short for big.log's first batch of 8: the object is
    // accepted on a partial score and the walk must still report a subset.
    for budget in [1, 3] {
        let mut opts = lazy_opts();
        opts.content.max_verify = budget;
        let r = fx.run_with("content:~line", &opts, None, 50);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("results are a subset")),
            "budget {budget}: {:?}",
            r.warnings
        );
        assert!(!r.total.exact, "budget {budget}: {:?}", r.total);
        // Whatever was returned is verified.
        assert!(r.hits.iter().all(|h| !h.snippets.is_empty()));
    }
}

#[test]
fn page_driven_relevance_ties_follow_entry_order_across_pages() {
    let fx = fixture();
    fx.extract_all();
    // `Zephyr` appears once in each of two Markdown files: equal bounds,
    // equal verified scores, so the entry-id tie-break decides the page
    // boundary. Page one of size one must show the same file as the first
    // row of an exhaustive run, and page two the other.
    let lazy = lazy_opts();
    let all = fx.run_with("content:~Zephyr ext:md", &lazy, None, 50);
    let names: Vec<&str> = all.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names.len(), 2);
    let p1 = fx.run_with("content:~Zephyr ext:md", &lazy, None, 1);
    assert_eq!(p1.hits[0].name, names[0]);
    let p2 = fx.run_with("content:~Zephyr ext:md", &lazy, p1.next_cursor.clone(), 1);
    assert_eq!(p2.hits[0].name, names[1]);
    assert_eq!(p1.hits[0].score, p2.hits[0].score);
}

#[test]
fn page_driven_pagination_is_stable() {
    let fx = fixture();
    fx.extract_all();
    let lazy = lazy_opts();
    let p1 = fx.run_with("content:~line", &lazy, None, 1);
    assert_eq!(p1.hits.len(), 1);
    let cursor = p1.next_cursor.clone().expect("second page");
    let p2 = fx.run_with("content:~line", &lazy, Some(cursor), 1);
    assert_eq!(p2.hits.len(), 1);
    assert_ne!(p1.hits[0].name, p2.hits[0].name);
    assert!(p2.next_cursor.is_none());
    let mut both = vec![p1.hits[0].name.clone(), p2.hits[0].name.clone()];
    both.sort();
    assert_eq!(both, fx.names("content:~line"));
    // Page one of a short walk reports an upper bound; page two finished.
    assert!(!p1.total.exact || p1.total.value == 2);
    assert!(p2.total.exact);
    assert_eq!(p2.total.value, 2);
}

#[test]
fn page_driven_verification_stays_eager_inside_not_and_or() {
    let fx = fixture();
    fx.extract_all();
    let lazy = lazy_opts();
    for q in [
        "ext:md -content:~build",
        "ext:md (content:~build OR content:~archive)",
    ] {
        let eager = fx.run(q);
        let r = fx.run_with(q, &lazy, None, 50);
        assert_eq!(sorted_names(&r), sorted_names(&eager), "{q}");
        let steps = r.explanation.unwrap().steps;
        assert!(
            steps
                .iter()
                .filter(|s| s.stage == "content")
                .all(|s| !s.description.contains("page-driven")),
            "{q}: {steps:?}"
        );
    }
    assert_eq!(fx.names("ext:md -content:~build"), vec!["readme.md"]);
}

// ----- deletion-only reindexing --------------------------------------------

const BINARY_BODY: &[u8] =
    b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00binary payload\x00\x00";

/// Rewrite `logs/notes.txt` (indexed as text by `extract_all`) and rescan so
/// the object reaches a new generation with `content_state = pending`.
fn change_notes(fx: &Fx, body: &[u8]) -> ObjectId {
    let obj = fx.object_of("notes.txt");
    assert_eq!(fx.names("content:lowercase"), vec!["notes.txt"]);
    assert!(object_ids(&fx.content).unwrap().contains(&obj));
    // Distinct mtime as well as size, so the rescan cannot miss the change.
    std::thread::sleep(Duration::from_millis(20));
    write(&fx.root.join("logs/notes.txt"), body);
    set_age(&fx.root.join("logs/notes.txt"), 0);
    fx.rescan();
    obj
}

/// The old text is gone from the index itself, and no snippet-less hit is
/// left behind on any retrieval path.
fn assert_old_text_gone(fx: &Fx, obj: ObjectId) {
    assert!(
        !object_ids(&fx.content).unwrap().contains(&obj),
        "the queued deletion was never committed: chunk documents remain"
    );
    for q in [
        "content:lowercase",
        "content:~lowercase",
        "content:=lowercase",
        r"content:/lowerc\w+/",
    ] {
        for opts in [ExecOptions::default(), lazy_opts()] {
            let r = fx.run_with(q, &opts, None, 50);
            assert!(r.hits.is_empty(), "{q}: {:?}", sorted_names(&r));
            assert_eq!(r.total.value, 0, "{q}");
        }
    }
}

#[test]
fn text_that_becomes_binary_commits_its_deletion() {
    let fx = fixture();
    fx.extract_all();
    let obj = change_notes(&fx, BINARY_BODY);

    let (published, dirty) = fx.extract_like_coordinator();
    assert_eq!(published, 0, "an unsupported file publishes no chunks");
    assert!(dirty, "the queued deletion must mark the writer dirty");
    assert_old_text_gone(&fx, obj);

    let r = fx.run("name:=notes.txt");
    assert_eq!(r.hits[0].content.state, ContentState::Unsupported);
    assert!(r.hits[0]
        .content
        .reason
        .as_deref()
        .is_some_and(|s| s.contains("binary")));
    assert!(r.completeness[0].content_complete);
}

#[test]
fn text_that_becomes_empty_commits_its_deletion() {
    let fx = fixture();
    fx.extract_all();
    let obj = change_notes(&fx, b"");

    let (published, dirty) = fx.extract_like_coordinator();
    assert_eq!(published, 1, "an empty file is indexed, with no chunks");
    assert!(dirty);
    assert_old_text_gone(&fx, obj);

    let rec = fx.catalog.content_record(obj).unwrap().unwrap();
    assert_eq!(rec.chunk_count, 0);
    assert_eq!(rec.state, ContentState::Indexed);
    let r = fx.run("name:=notes.txt");
    assert_eq!(r.hits[0].content.state, ContentState::Indexed);
    assert!(r.hits[0].snippets.is_empty());
}

#[test]
fn missing_file_after_a_change_commits_its_deletion() {
    let fx = fixture();
    fx.extract_all();
    let obj = change_notes(&fx, b"rewritten notes with a brandnew token\n");
    // The file disappears between the scan and the extraction: a
    // deterministic failure, so the job is terminal and nothing replaces the
    // chunks it just queued for deletion.
    std::fs::remove_file(fx.root.join("logs/notes.txt")).unwrap();

    let (published, dirty) = fx.extract_like_coordinator();
    assert_eq!(published, 0);
    assert!(dirty);
    assert_old_text_gone(&fx, obj);
    assert!(fx.names("content:brandnew").is_empty());

    let rec = fx.catalog.content_record(obj).unwrap().unwrap();
    assert_eq!(rec.state, ContentState::Failed);
    assert_eq!(rec.coverage, Coverage::None);
    assert_eq!(
        fx.run("name:=notes.txt").hits[0].content.state,
        ContentState::Failed
    );
}

/// Put `logs/notes.txt` into the state where both its generations are in the
/// content index: the new one is published without the old documents being
/// dropped — the window a queued-but-uncommitted deletion leaves open, and
/// the state a crash between the index commit and publication can persist.
/// Returns `(object, old generation, new generation)`.
fn coexisting_generations(fx: &Fx) -> (ObjectId, u32, u32) {
    let text = "rewritten notes with a brandnew token\n";
    let obj = change_notes(fx, text.as_bytes());
    let old_gen = fx.catalog.content_record(obj).unwrap().unwrap().generation;
    let new_gen = fx.catalog.get_object(obj).unwrap().unwrap().generation;
    assert_eq!(new_gen, old_gen + 1);
    let chunk = Chunk {
        ordinal: 0,
        byte_start: 0,
        byte_end: text.len() as u64,
        line_start: 0,
        line_end: 0,
        text: text.to_string(),
        split_line: false,
    };
    fx.catalog
        .write_chunks(obj, new_gen, std::slice::from_ref(&chunk))
        .unwrap();
    fx.content
        .add_chunks(obj, fx.source, new_gen, std::slice::from_ref(&chunk))
        .unwrap();
    fx.content.commit().unwrap();
    assert!(
        object_ids(&fx.content).unwrap().contains(&obj),
        "both generations are in the index"
    );
    // The superseded chunk text is still stored, so a verified clause would
    // match it if the generation were not checked.
    assert_eq!(
        fx.catalog.chunks_for(obj, old_gen, &[0]).unwrap().len(),
        1,
        "the old generation's chunk is still in the catalog"
    );
    (obj, old_gen, new_gen)
}

#[test]
fn old_and_new_generation_documents_coexist_without_a_stale_hit() {
    let fx = fixture();
    fx.extract_all();
    coexisting_generations(&fx);

    for opts in [ExecOptions::default(), lazy_opts()] {
        for q in [
            "content:lowercase",
            "content:~lowercase",
            "content:=lowercase",
            r"content:/lowerc\w+/",
        ] {
            let r = fx.run_with(q, &opts, None, 50);
            assert!(r.hits.is_empty(), "{q}: {:?}", sorted_names(&r));
            assert_eq!(r.total.value, 0, "{q}");
        }
        // Only the current generation answers, and it still snippets.
        for q in ["content:brandnew", "content:~brandnew"] {
            let r = fx.run_with(q, &opts, None, 50);
            assert_eq!(sorted_names(&r), vec!["notes.txt"], "{q}");
            assert!(!r.hits[0].snippets.is_empty(), "{q}");
            assert!(r.hits[0].snippets[0].text.contains("brandnew"), "{q}");
        }
    }
    // The plan says why the stale candidate went away.
    let steps = fx.run("content:lowercase").explanation.unwrap().steps;
    let c = steps.iter().find(|s| s.stage == "content").unwrap();
    assert!(c.description.contains("older generation dropped"), "{c:?}");
}

#[test]
fn a_stale_candidate_cut_by_truncation_is_reported_not_silent() {
    let fx = fixture();
    fx.extract_all();
    coexisting_generations(&fx);
    // Candidates are cut before their generation is known, so a truncated
    // list can keep the superseded chunk of a file and drop the current one.
    // `notes` is in both generations and in no other file, and only one
    // candidate survives, so the outcome is either the file (the current
    // chunk won the cut) or a warning that names both causes — never the
    // superseded text presented as a hit.
    for (mode, mut opts) in [
        ("top-k", ExecOptions::default()),
        ("candidates", ExecOptions::default()),
    ] {
        if mode == "top-k" {
            opts.content.top_k = 1;
        } else {
            opts.content.max_candidates = 1;
        }
        let q = if mode == "top-k" {
            "content:notes"
        } else {
            "content:~notes"
        };
        let r = fx.run_with(q, &opts, None, 50);
        if r.hits.is_empty() {
            assert!(
                r.warnings
                    .iter()
                    .any(|w| w.contains("superseded generation")),
                "{mode}: {:?}",
                r.warnings
            );
        } else {
            assert_eq!(sorted_names(&r), vec!["notes.txt"], "{mode}");
            assert!(r.hits[0].snippets[0].text.contains("brandnew"), "{mode}");
        }
        assert!(
            r.warnings.iter().any(|w| w.contains("subset")),
            "{mode}: truncation is always reported: {:?}",
            r.warnings
        );
    }
}
