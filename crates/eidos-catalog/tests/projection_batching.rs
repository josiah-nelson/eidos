//! Projection reads are batched: a rebuild must not query per directory and
//! a subtree rebuild must not walk each descendant's ancestor chain on its
//! own. Every test compares the batched output against the pre-batching
//! reference implementation kept in `projection::reference`, so the
//! optimisation cannot change what the search index sees.

use eidos_catalog::projection::{query_count, reset_query_count, ProjectionRow, PROJECTION_BATCH};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{ObjectId, SourceId, SourceKind};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Tracks live Rust heap bytes so the ignored benchmark can report the peak
/// a rebuild holds. SQLite's own allocations are not routed through this.
struct Tracking;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// Reset the peak to the bytes currently live; returns that baseline.
fn arm_peak() -> usize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

fn peak_since(baseline: usize) -> usize {
    PEAK.load(Ordering::Relaxed).saturating_sub(baseline)
}

struct Fx {
    _dir: tempfile::TempDir,
    catalog: Arc<Catalog>,
    source: SourceId,
}

fn fixture(build: impl FnOnce(&Path)) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    build(&root);
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
    Fx {
        _dir: dir,
        catalog,
        source,
    }
}

fn write(path: PathBuf, bytes: usize) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A little of everything: nesting, several extensions per directory, an
/// empty directory, and a file at the source root.
fn mixed(root: &Path) {
    write(root.join("proj/src/util/Helpers.cs"), 20);
    write(root.join("proj/src/Program.cs"), 30);
    write(root.join("proj/src/notes.md"), 10);
    write(root.join("proj/ida/sample.idb"), 40);
    write(root.join("other/app/Main.cs"), 50);
    write(root.join("other/notes.txt"), 10);
    write(root.join("top.log"), 5);
    std::fs::create_dir_all(root.join("empty")).unwrap();
}

/// `depth` nested single-character directories, each holding one file.
fn deep(root: &Path, depth: usize) {
    let mut cur = root.to_path_buf();
    for i in 0..depth {
        cur = cur.join("d");
        write(cur.join(format!("f{i}.txt")), 3);
    }
}

/// `dirs` sibling directories, each holding two files of different kinds.
fn wide(root: &Path, dirs: usize) {
    for i in 0..dirs {
        write(root.join(format!("d{i}/a.txt")), 3);
        write(root.join(format!("d{i}/b.log")), 3);
    }
}

impl Fx {
    fn batched_rows(&self) -> Vec<ProjectionRow> {
        let mut out = Vec::new();
        self.catalog
            .for_each_projection_row(self.source, |row| {
                out.push(row);
                Ok(())
            })
            .unwrap();
        out
    }

    fn reference_rows(&self) -> Vec<ProjectionRow> {
        let mut out = Vec::new();
        self.catalog
            .reference_for_each_projection_row(self.source, |row| {
                out.push(row);
                Ok(())
            })
            .unwrap();
        out
    }

    fn live_objects(&self) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self
            .batched_rows()
            .into_iter()
            .map(|r| r.object_id)
            .collect();
        ids.sort_unstable_by_key(|o| o.0);
        ids.dedup();
        ids
    }
}

fn by_entry(mut rows: Vec<ProjectionRow>) -> Vec<ProjectionRow> {
    rows.sort_by_key(|r| r.entry_id);
    rows
}

#[test]
fn batched_reads_match_the_reference_row_for_row() {
    let fx = fixture(mixed);
    let rows = fx.batched_rows();
    assert!(rows.len() > 10, "fixture too small: {}", rows.len());
    assert_eq!(rows, fx.reference_rows());
    for object in fx.live_objects() {
        let batched = fx.catalog.projection_rows_for_object(object).unwrap();
        let reference = fx
            .catalog
            .reference_projection_rows_for_object(object)
            .unwrap();
        assert_eq!(batched, reference, "object {object:?}");
    }
}

/// Queries a rebuild is allowed: source lookup, path-node map, the row
/// statement, and one descendant-extension query per batch.
fn rebuild_budget(rows: usize) -> u64 {
    (rows.div_ceil(PROJECTION_BATCH) + 4) as u64
}

#[test]
fn deep_tree_rebuild_is_batch_bounded() {
    let depth = 40;
    let fx = fixture(|root| deep(root, depth));
    reset_query_count();
    let rows = fx.batched_rows();
    let queries = query_count();
    assert_eq!(rows.len(), 2 * depth + 1, "one dir and one file per level");
    assert!(
        queries <= rebuild_budget(rows.len()),
        "{queries} queries for {} rows (budget {})",
        rows.len(),
        rebuild_budget(rows.len())
    );
    reset_query_count();
    let _ = fx.reference_rows();
    let reference = query_count();
    assert!(
        reference >= depth as u64,
        "reference should query per directory, got {reference}"
    );
}

#[test]
fn wide_tree_rebuild_is_batch_bounded() {
    let dirs = 1500;
    let fx = fixture(|root| wide(root, dirs));
    reset_query_count();
    let rows = fx.batched_rows();
    let queries = query_count();
    assert_eq!(rows.len(), 3 * dirs + 1);
    assert!(
        queries <= rebuild_budget(rows.len()),
        "{queries} queries for {} rows (budget {})",
        rows.len(),
        rebuild_budget(rows.len())
    );
    reset_query_count();
    let reference_rows = fx.reference_rows();
    let reference = query_count();
    // Several batches of output, and every row still identical.
    assert!(rows.len() > 4 * PROJECTION_BATCH);
    assert_eq!(rows, reference_rows);
    assert!(
        reference > 10 * queries,
        "batched {queries} vs reference {reference}"
    );
}

#[test]
fn subtree_rebuild_walks_each_ancestor_once() {
    // 300 directories five levels down, each with two files: the shape a
    // moved or renamed subtree produces on the follower.
    let fx = fixture(|root| {
        for i in 0..300 {
            write(root.join(format!("a/b/c/moved/d{i}/one.txt")), 3);
            write(root.join(format!("a/b/c/moved/d{i}/two.log")), 3);
        }
    });
    let moved = fx
        .catalog
        .resolve_relative(fx.source, "a/b/c/moved")
        .unwrap()
        .unwrap();
    let mut objects = vec![moved];
    objects.extend(fx.catalog.descendant_object_ids(moved).unwrap());
    assert_eq!(objects.len(), 901);

    reset_query_count();
    let mut batched = Vec::new();
    fx.catalog
        .for_each_projection_row_of(&objects, |row| {
            batched.push(row);
            Ok(())
        })
        .unwrap();
    let queries = query_count();

    reset_query_count();
    let mut reference = Vec::new();
    for object in &objects {
        reference.extend(
            fx.catalog
                .reference_projection_rows_for_object(*object)
                .unwrap(),
        );
    }
    let reference_queries = query_count();

    assert_eq!(by_entry(batched), by_entry(reference));
    // One preload, one row query and one extension query per batch, plus the
    // handful of lookups for the ancestors above the subtree.
    let budget = (objects.len().div_ceil(PROJECTION_BATCH) * 3 + 12) as u64;
    assert!(
        queries <= budget,
        "{queries} queries for {} objects (budget {budget})",
        objects.len()
    );
    // The reference count is a lower bound: it does not instrument the path
    // walk inside `render_path_conn`.
    assert!(
        reference_queries > 20 * queries,
        "batched {queries} vs reference {reference_queries}"
    );
}

/// Elapsed time, peak Rust heap, and query count of a full rebuild and of a
/// large subtree rebuild, old path versus new. Two passes; the second is
/// reported, so the page cache is warm for both. Run with:
/// `cargo test -p eidos-catalog --release --test projection_batching -- --ignored --nocapture --test-threads=1`
#[test]
#[ignore]
fn bench_reference_versus_batched() {
    // ~36k entries: 6000 directories of five files each, six levels down so
    // the subtree pass has an ancestor chain to walk.
    let fx = fixture(|root| {
        for i in 0..6000 {
            for j in 0..5 {
                write(root.join(format!("a/b/c/moved/d{i}/f{j}.txt")), 1);
            }
        }
    });
    let moved = fx
        .catalog
        .resolve_relative(fx.source, "a/b/c/moved")
        .unwrap()
        .unwrap();
    let mut objects = vec![moved];
    objects.extend(fx.catalog.descendant_object_ids(moved).unwrap());

    for pass in 0..2 {
        let report = |label: &str, rows: u64, started: Instant, base: usize| {
            if pass == 1 {
                println!(
                    "{label}: {rows} rows, {:?}, peak {} B, {} queries",
                    started.elapsed(),
                    peak_since(base),
                    query_count()
                );
            }
        };

        let (base, started) = (arm_peak(), Instant::now());
        let mut rows = 0u64;
        reset_query_count();
        fx.catalog
            .reference_for_each_projection_row(fx.source, |_| {
                rows += 1;
                Ok(())
            })
            .unwrap();
        report("rebuild reference", rows, started, base);

        let (base, started) = (arm_peak(), Instant::now());
        let mut rows = 0u64;
        reset_query_count();
        fx.catalog
            .for_each_projection_row(fx.source, |_| {
                rows += 1;
                Ok(())
            })
            .unwrap();
        report("rebuild batched  ", rows, started, base);

        let (base, started) = (arm_peak(), Instant::now());
        let mut rows = 0u64;
        reset_query_count();
        for object in &objects {
            rows += fx
                .catalog
                .reference_projection_rows_for_object(*object)
                .unwrap()
                .len() as u64;
        }
        report("subtree reference", rows, started, base);

        let (base, started) = (arm_peak(), Instant::now());
        let mut rows = 0u64;
        reset_query_count();
        fx.catalog
            .for_each_projection_row_of(&objects, |_| {
                rows += 1;
                Ok(())
            })
            .unwrap();
        report("subtree batched  ", rows, started, base);
    }
}
