//! Fault injection for the content pipeline's chunk sink: a SQLite write
//! failure, a content-index write failure after chunks were already
//! flushed, and the retry that then succeeds.
//!
//! The invariants under test (issue #2): a sink failure is classified
//! `transient` and retried under the job's backoff instead of becoming a
//! terminal extraction failure; a failed attempt leaves nothing searchable
//! behind; the operator-visible reason keeps the underlying error chain;
//! and the retry produces exactly one generation's chunks.

#![cfg(windows)]

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_content::{Chunk, Limits};
use eidos_domain::*;
use eidos_query::parse;
use eidos_search::content::object_ids;
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_search::pipeline::{drain_content_jobs_with_faults, SinkFaults, SinkStage};
use eidos_search::{CatalogIndex, ContentIndex};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Lines of ~48 bytes; enough for well over two batches of `CHUNK_BATCH`
/// (64) 16 KiB chunks, so a failure can land after flushed chunks.
const LINES: usize = 48_000;

struct Fx {
    _dir: tempfile::TempDir,
    log: PathBuf,
    catalog: Arc<Catalog>,
    index: Arc<CatalogIndex>,
    content: Arc<ContentIndex>,
    source: SourceId,
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let mut body = String::with_capacity(LINES * 48);
    for i in 0..LINES {
        match i {
            3 => body.push_str("2026-08-22 12:00:00 WARN qzalpha opened the journal\n"),
            _ if i == LINES - 4 => {
                body.push_str("2026-08-22 12:00:00 WARN qzomega closed the journal\n")
            }
            _ => body.push_str(&format!(
                "2026-08-22 12:00:00 INFO routine line number {i}\n"
            )),
        }
    }
    let log = root.join("data/journal.log");
    std::fs::write(&log, &body).unwrap();

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
        log,
        catalog,
        index,
        content,
        source,
    }
}

/// One `content_text` job, as the coordinator would queue it.
#[derive(Debug, PartialEq)]
struct JobRow {
    state: String,
    attempts: u32,
    failure_class: Option<String>,
    last_error: Option<String>,
}

impl Fx {
    fn object(&self) -> ObjectId {
        let conn = self
            .catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap();
        ObjectId(
            conn.query_row(
                "SELECT object_id FROM objects WHERE kind = 'file' AND deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        )
    }

    fn generation(&self) -> u32 {
        self.catalog
            .content_target(self.object())
            .unwrap()
            .unwrap()
            .generation
    }

    fn enqueue(&self) {
        assert_eq!(
            self.catalog
                .enqueue_pending_content(self.source, 100)
                .unwrap(),
            1
        );
    }

    /// Make every chunk insert at or beyond `ordinal` fail, the way a
    /// SQLite write error would (the trigger is installed on a second
    /// connection, outside the pipeline).
    fn fail_chunk_inserts_from(&self, ordinal: u32) {
        self.catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER chunk_write_fault BEFORE INSERT ON chunks WHEN NEW.ordinal >= {ordinal}
                 BEGIN SELECT RAISE(ABORT, 'simulated disk I/O error'); END;"
            ))
            .unwrap();
    }

    fn repair_chunk_writes(&self) {
        self.catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap()
            .execute_batch("DROP TRIGGER chunk_write_fault;")
            .unwrap();
    }

    /// Skip the retry backoff so the next drain claims the job.
    fn due_now(&self) {
        self.catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap()
            .execute(
                "UPDATE jobs SET scheduled_at = 0 WHERE state = 'queued'",
                [],
            )
            .unwrap();
    }

    fn job(&self) -> JobRow {
        let conn = self
            .catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap();
        conn.query_row(
            "SELECT state, attempts, failure_class, last_error FROM jobs WHERE stage = 'content_text'",
            [],
            |r| {
                Ok(JobRow {
                    state: r.get(0)?,
                    attempts: r.get::<_, i64>(1)? as u32,
                    failure_class: r.get(2)?,
                    last_error: r.get(3)?,
                })
            },
        )
        .unwrap()
    }

    /// `(rows, distinct generations)` of stored chunks for the object.
    fn stored_chunks(&self) -> (u32, u32) {
        let conn = self
            .catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap();
        conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT generation) FROM chunks WHERE object_id = ?1",
            [self.object().0],
            |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32)),
        )
        .unwrap()
    }

    fn content_state(&self) -> ContentState {
        let conn = self
            .catalog
            .open_uncoordinated_writer_for_fault_injection()
            .unwrap();
        let s: String = conn
            .query_row(
                "SELECT content_state FROM objects WHERE object_id = ?1",
                [self.object().0],
                |r| r.get(0),
            )
            .unwrap();
        ContentState::parse(&s).unwrap()
    }

    fn drain(&self) -> u64 {
        self.drain_with(&SinkFaults::default())
    }

    fn drain_with(&self, faults: &SinkFaults) -> u64 {
        let n = drain_content_jobs_with_faults(
            &self.catalog,
            &self.content,
            &Limits::default(),
            "test",
            faults,
        )
        .unwrap();
        self.index.follow_once(&self.catalog, 10_000).unwrap();
        n
    }

    fn hits(&self, q: &str) -> Vec<Hit> {
        let parsed = parse(q).unwrap();
        let mut r = SearchRequest::new(parsed.query);
        r.limit = 50;
        search_with_content(
            &self.index,
            Some(&self.content),
            &self.catalog,
            &r,
            &ExecOptions::default(),
        )
        .unwrap()
        .hits
    }

    /// Nothing this attempt wrote is stored or searchable.
    fn assert_no_partial_output(&self) {
        assert_eq!(self.stored_chunks(), (0, 0), "chunk rows left behind");
        assert!(
            object_ids(&self.content).unwrap().is_empty(),
            "documents left in the content index"
        );
        assert_eq!(self.content.num_docs(), 0);
        assert!(
            self.catalog
                .content_record(self.object())
                .unwrap()
                .is_none(),
            "a failed attempt must not leave a content record to publish"
        );
        assert_eq!(self.content_state(), ContentState::Pending);
        assert!(self.hits("content:qzalpha").is_empty(), "early chunk");
        assert!(self.hits("content:qzomega").is_empty(), "late chunk");
    }
}

#[test]
fn catalog_write_failure_is_retryable_and_leaves_nothing_searchable() {
    let fx = fixture();
    fx.enqueue();
    fx.fail_chunk_inserts_from(64);

    assert_eq!(fx.drain(), 0, "nothing published");

    let job = fx.job();
    assert_eq!(job.state, "queued", "requeued for the backoff, not failed");
    assert_eq!(job.attempts, 1);
    assert_eq!(
        job.failure_class.as_deref(),
        Some("transient"),
        "a storage failure is not a deterministic extraction failure"
    );
    let error = job.last_error.unwrap();
    assert!(
        error.starts_with("catalog write failed for object"),
        "{error}"
    );
    assert!(
        error.contains("simulated disk I/O error"),
        "the underlying error chain survives: {error}"
    );
    fx.assert_no_partial_output();
}

#[test]
fn index_write_failure_after_flushed_chunks_is_retryable_and_discards_them() {
    let fx = fixture();
    fx.enqueue();
    // Fail the index write only once a whole batch has been flushed to
    // both stores, so the attempt really does leave partial output behind.
    let flushed = Arc::new(AtomicU32::new(0));
    let seen = flushed.clone();
    let faults = SinkFaults::new(move |stage, written| {
        seen.fetch_max(written, Ordering::Relaxed);
        (stage == SinkStage::Index && written >= 64)
            .then(|| "simulated index writer failure".to_string())
    });

    assert_eq!(fx.drain_with(&faults), 0);
    assert!(
        flushed.load(Ordering::Relaxed) >= 64,
        "at least one batch was written before the failure"
    );

    let job = fx.job();
    assert_eq!(job.state, "queued");
    assert_eq!(job.attempts, 1);
    assert_eq!(job.failure_class.as_deref(), Some("transient"));
    let error = job.last_error.unwrap();
    assert!(
        error.starts_with("index write failed for object"),
        "{error}"
    );
    assert!(error.contains("simulated index writer failure"), "{error}");
    fx.assert_no_partial_output();
}

#[test]
fn retry_after_a_sink_failure_indexes_exactly_one_generation() {
    let fx = fixture();
    fx.enqueue();
    fx.fail_chunk_inserts_from(64);
    assert_eq!(fx.drain(), 0);
    fx.assert_no_partial_output();

    // The store recovers; the job's next attempt is due.
    fx.repair_chunk_writes();
    fx.due_now();
    assert_eq!(fx.drain(), 1, "published on the retry");

    let rec = fx.catalog.content_record(fx.object()).unwrap().unwrap();
    assert_eq!(rec.state, ContentState::Indexed);
    assert_eq!(rec.coverage, Coverage::Full);
    assert_eq!(rec.line_count, LINES as u64);
    assert!(rec.chunk_count > 64, "{rec:?}");
    assert_eq!(rec.generation, fx.generation());
    assert!(rec.hash_complete);
    assert_eq!(
        fx.stored_chunks(),
        (rec.chunk_count, 1),
        "exactly one generation's chunks, and all of them"
    );
    assert_eq!(
        fx.content.num_docs(),
        rec.chunk_count as u64,
        "one live document per stored chunk"
    );
    assert_eq!(fx.content_state(), ContentState::Indexed);

    let job = fx.job();
    assert_eq!(job.state, "done");
    assert_eq!(job.attempts, 2);

    for (q, line) in [("content:qzalpha", 3), ("content:qzomega", LINES - 4)] {
        let hits = fx.hits(q);
        assert_eq!(hits.len(), 1, "{q}");
        assert_eq!(hits[0].name, "journal.log");
        let s = &hits[0].snippets[0];
        assert_eq!(s.line_start as usize, line, "{q}: {s:?}");
    }
}

#[test]
fn a_content_record_never_claims_chunks_that_are_not_stored() {
    let fx = fixture();
    fx.enqueue();
    assert_eq!(fx.drain(), 1);
    let object = fx.object();
    let rec = fx.catalog.content_record(object).unwrap().unwrap();

    // An attempt that died mid-flight (or one made under wider limits) left
    // rows beyond what the record claims for this generation.
    let stray = Chunk {
        ordinal: rec.chunk_count + 1,
        byte_start: 0,
        byte_end: 4,
        line_start: 0,
        line_end: 0,
        text: "qzstray\n".into(),
        split_line: false,
    };
    fx.catalog
        .write_chunks(object, rec.generation, &[stray])
        .unwrap();
    assert_eq!(fx.stored_chunks(), (rec.chunk_count + 1, 1));

    // Storing the record drops everything it does not account for.
    fx.catalog.store_content(&rec, &[], true, None).unwrap();
    assert_eq!(fx.stored_chunks(), (rec.chunk_count, 1));

    // And discarding a generation is idempotent.
    assert_eq!(
        fx.catalog.delete_chunks(object, rec.generation).unwrap(),
        rec.chunk_count as u64
    );
    assert_eq!(fx.catalog.delete_chunks(object, rec.generation).unwrap(), 0);
}

#[test]
fn a_deterministic_extraction_failure_stays_terminal() {
    let fx = fixture();
    fx.enqueue();
    // The file is gone by the time the worker opens it: that is a property
    // of the source, not of the store, and must not be retried forever.
    std::fs::remove_file(&fx.log).unwrap();

    assert_eq!(fx.drain(), 0);

    let job = fx.job();
    assert_eq!(job.state, "done", "the outcome is recorded, not retried");
    assert_eq!(job.attempts, 1);
    let rec = fx.catalog.content_record(fx.object()).unwrap().unwrap();
    assert_eq!(rec.state, ContentState::Failed);
    assert_eq!(rec.failure_class, Some(FailureClass::Deterministic));
    assert_eq!(rec.coverage, Coverage::None);
    assert_eq!(rec.chunk_count, 0);
    assert!(rec.error.unwrap().contains("open:"));
    assert_eq!(fx.content_state(), ContentState::Failed);
    assert_eq!(fx.stored_chunks(), (0, 0));
}
