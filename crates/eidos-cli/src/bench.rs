//! `eidos bench search` — in-process query latency benchmark against an
//! existing data directory, optionally while a rescan runs concurrently.
//! Emits an `eidos-bench/1` record with p50/p95/p99 per query family.

use anyhow::Context;
use clap::{Args, Subcommand};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::Catalog;
use eidos_domain::bench::{BenchTimer, LatencySamples};
use eidos_domain::{ResultMode, SearchRequest, Sort, SortField};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_search::{CatalogIndex, ContentIndex};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Args, Debug)]
pub struct BenchArgs {
    #[arg(long, env = "EIDOS_DATA_DIR", default_value = "data", global = true)]
    pub data_dir: PathBuf,
    #[command(subcommand)]
    pub command: BenchCommand,
}

#[derive(Subcommand, Debug)]
pub enum BenchCommand {
    /// Query latency over the catalog index.
    Search {
        /// Iterations per query.
        #[arg(long, default_value_t = 30)]
        iterations: u32,
        /// Also run a full rescan of this source (by name) while querying.
        #[arg(long)]
        concurrent_scan: Option<String>,
        /// Append the benchmark record to this JSONL file.
        #[arg(long)]
        bench_out: Option<PathBuf>,
        /// Extra queries (eidos syntax) to include, family "custom".
        #[arg(long)]
        query: Vec<String>,
        /// Rebuild the index from the catalog before measuring.
        #[arg(long)]
        rebuild: bool,
    },
    /// Time stored-chunk lookups (`chunks_for`) serially and in parallel.
    Chunks {
        #[arg(long, default_value_t = 2000)]
        sample: u32,
        #[arg(long, default_value_t = 8)]
        threads: usize,
    },
    /// Stream one file through the extractor (sniff/decode/chunk/hash) with a
    /// null sink; reports throughput and peak working set. Read-only.
    Content {
        /// File to extract.
        file: PathBuf,
        /// Append the benchmark record to this JSONL file.
        #[arg(long)]
        bench_out: Option<PathBuf>,
        /// Workload label for the record (defaults to the file name).
        #[arg(long)]
        label: Option<String>,
    },
}

struct Case {
    family: &'static str,
    text: &'static str,
    mode: ResultMode,
    sort: SortField,
}

const CASES: &[Case] = &[
    Case {
        family: "metadata",
        text: "ext:cs",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "metadata",
        text: "ext:dmp",
        mode: ResultMode::Files,
        sort: SortField::Size,
    },
    Case {
        family: "metadata",
        text: "size:>1G",
        mode: ResultMode::Files,
        sort: SortField::Size,
    },
    Case {
        family: "metadata",
        text: "mtime:>=30d ext:log",
        mode: ResultMode::Files,
        sort: SortField::Modified,
    },
    Case {
        family: "metadata",
        text: "ext:log size:>100M",
        mode: ResultMode::Files,
        sort: SortField::Size,
    },
    Case {
        family: "metadata",
        text: "state:excluded",
        mode: ResultMode::Files,
        sort: SortField::Name,
    },
    Case {
        family: "name",
        text: "readme",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "name",
        text: "name:config",
        mode: ResultMode::Files,
        sort: SortField::Name,
    },
    Case {
        family: "name",
        text: "*.json",
        mode: ResultMode::Files,
        sort: SortField::Size,
    },
    Case {
        family: "name",
        text: "setup config registry",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "name",
        text: "name:=README.md",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "regex",
        text: "name:/^\\d{1,3}(\\.\\d{1,3}){3}( \\(\\d+\\))?$/",
        mode: ResultMode::Directories,
        sort: SortField::SubtreeSize,
    },
    Case {
        family: "regex",
        text: "name:/postgresql-.*\\.log$/",
        mode: ResultMode::Files,
        sort: SortField::Size,
    },
    Case {
        family: "regex",
        text: "name:/^[A-Z]{3}-\\d{4}/c",
        mode: ResultMode::Files,
        sort: SortField::Name,
    },
    Case {
        family: "regex",
        text: "path:/\\\\bin\\\\/ ext:dll",
        mode: ResultMode::Files,
        sort: SortField::Name,
    },
    Case {
        family: "directory",
        text: "has:idb has:cs",
        mode: ResultMode::Directories,
        sort: SortField::Relevance,
    },
    Case {
        family: "directory",
        text: "kind:dir subtree:>1G",
        mode: ResultMode::Directories,
        sort: SortField::SubtreeSize,
    },
    Case {
        family: "directory",
        text: "kind:dir files:>1000",
        mode: ResultMode::Directories,
        sort: SortField::Name,
    },
    Case {
        family: "content",
        text: "content:error",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "content",
        text: "content:\"connection refused\"",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "content",
        text: "content:exception ext:log mtime:>=365d",
        mode: ResultMode::Files,
        sort: SortField::Modified,
    },
    Case {
        family: "content-exact",
        text: "content:=Exception",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "content-exact",
        text: "content:~localhost:",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "content-regex",
        text: "content:/timed? ?out after \\d+/",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "content-regex",
        text: "content:/[A-Z][a-z]+Exception: /c ext:log",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
];

pub fn run(args: BenchArgs) -> anyhow::Result<()> {
    match args.command {
        BenchCommand::Search {
            iterations,
            concurrent_scan,
            bench_out,
            query,
            rebuild,
        } => bench_search(
            &args.data_dir,
            iterations,
            concurrent_scan,
            bench_out,
            query,
            rebuild,
        ),
        BenchCommand::Content {
            file,
            bench_out,
            label,
        } => bench_content(&file, bench_out, label),
        BenchCommand::Chunks { sample, threads } => bench_chunks(&args.data_dir, sample, threads),
    }
}

fn bench_chunks(data_dir: &std::path::Path, sample: u32, threads: usize) -> anyhow::Result<()> {
    let catalog = Catalog::open(data_dir.join("catalog.db"))?;
    let keys = catalog.sample_chunk_keys(sample)?;
    anyhow::ensure!(!keys.is_empty(), "no chunks stored");
    for pass in ["cold", "warm"] {
        let t = Instant::now();
        let mut bytes = 0usize;
        for (o, g, k) in &keys {
            for row in catalog.chunks_for(*o, *g, &[*k])? {
                bytes += row.text.len();
            }
        }
        let d = t.elapsed();
        println!(
            "serial {pass}: {} lookups in {:.0} ms = {:.0} us each ({} text bytes)",
            keys.len(),
            d.as_secs_f64() * 1000.0,
            d.as_secs_f64() * 1e6 / keys.len() as f64,
            bytes
        );
    }
    let per = keys.len().div_ceil(threads.max(1));
    let t = Instant::now();
    std::thread::scope(|s| {
        for part in keys.chunks(per) {
            let catalog = &catalog;
            s.spawn(move || {
                for (o, g, k) in part {
                    let _ = catalog.chunks_for(*o, *g, &[*k]);
                }
            });
        }
    });
    let d = t.elapsed();
    println!(
        "parallel x{threads} (one txn per lookup): {} lookups in {:.0} ms = {:.0} us each wall",
        keys.len(),
        d.as_secs_f64() * 1000.0,
        d.as_secs_f64() * 1e6 / keys.len() as f64
    );
    let t = Instant::now();
    std::thread::scope(|s| {
        for part in keys.chunks(per) {
            let catalog = &catalog;
            s.spawn(move || {
                for batch in part.chunks(256) {
                    let _ = catalog.chunks_for_many(batch);
                }
            });
        }
    });
    let d = t.elapsed();
    // Pure CPU scaling check on the same texts (no SQLite involved).
    let texts: Vec<String> = keys
        .chunks(256)
        .flat_map(|b| catalog.chunks_for_many(b).unwrap_or_default())
        .map(|r| r.text)
        .collect();
    let re = regex::Regex::new(r"[A-Z][a-z]+Exception: ").unwrap();
    let t = Instant::now();
    let mut hits = 0usize;
    for _ in 0..20 {
        for tx in &texts {
            hits += re.find_iter(tx).count();
        }
    }
    let serial = t.elapsed();
    let t = Instant::now();
    let per_t = texts.len().div_ceil(threads.max(1));
    let parallel_hits: usize = std::thread::scope(|s| {
        let hs: Vec<_> = texts
            .chunks(per_t)
            .map(|part| {
                let re = &re;
                s.spawn(move || {
                    let mut h = 0usize;
                    for _ in 0..20 {
                        for tx in part {
                            h += re.find_iter(tx).count();
                        }
                    }
                    h
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    let par = t.elapsed();
    println!(
        "cpu regex over {} texts x20: serial {:.0} ms, parallel x{threads} {:.0} ms (speedup {:.1}x, hits {hits}/{parallel_hits})",
        texts.len(),
        serial.as_secs_f64() * 1000.0,
        par.as_secs_f64() * 1000.0,
        serial.as_secs_f64() / par.as_secs_f64().max(1e-9)
    );
    println!(
        "parallel x{threads} (batched txns): {} lookups in {:.0} ms = {:.0} us each wall",
        keys.len(),
        d.as_secs_f64() * 1000.0,
        d.as_secs_f64() * 1e6 / keys.len() as f64
    );
    Ok(())
}

/// Peak working set of this process in bytes (Windows); 0 elsewhere.
fn peak_working_set() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: zero is a valid bit pattern for this plain C struct.
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: `c` is a correctly sized, writable struct; the handle is the
        // pseudo-handle of the current process.
        let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        if ok != 0 {
            return c.PeakWorkingSetSize as u64;
        }
        0
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn bench_content(
    file: &std::path::Path,
    bench_out: Option<PathBuf>,
    label: Option<String>,
) -> anyhow::Result<()> {
    use eidos_content::{extract, Limits};
    let label = label.unwrap_or_else(|| {
        file.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into())
    });
    let size = std::fs::metadata(file)?.len();
    let baseline_peak = peak_working_set();
    let mut timer = BenchTimer::start("content.stream", &label);
    let mut chunks = 0u64;
    let mut text_bytes = 0u64;
    let mut max_chunk = 0u64;
    let started = Instant::now();
    let outcome = extract(file, &Limits::default(), &mut |c| {
        chunks += 1;
        text_bytes += c.text.len() as u64;
        max_chunk = max_chunk.max(c.text.len() as u64);
        Ok(())
    });
    let elapsed = started.elapsed();
    let peak = peak_working_set();
    let mb_s = outcome.indexed_bytes as f64 / 1_048_576.0 / elapsed.as_secs_f64().max(1e-9);
    println!(
        "{label}: {} -> state {} coverage {} encoding {} in {:.1} s ({:.1} MB/s)",
        crate::profile::human_bytes(size),
        outcome.state,
        outcome.coverage,
        outcome.encoding.map(|e| e.as_str()).unwrap_or("-"),
        elapsed.as_secs_f64(),
        mb_s
    );
    println!(
        "chunks {chunks} (max {} decoded bytes), lines {}, chars {}, hash {}",
        crate::profile::human_bytes(max_chunk),
        outcome.line_count,
        outcome.chars,
        outcome
            .content_id
            .map(|c| c.to_hex())
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "peak working set {} (baseline before extraction {})",
        crate::profile::human_bytes(peak),
        crate::profile::human_bytes(baseline_peak)
    );
    if let Some((class, err)) = &outcome.failure {
        println!("failure: {class}: {err}");
        timer.fail();
    }
    timer
        .counter("file_bytes", size)
        .counter("indexed_bytes", outcome.indexed_bytes)
        .counter("chunks", chunks)
        .counter("text_bytes", text_bytes)
        .counter("max_chunk_bytes", max_chunk)
        .counter("lines", outcome.line_count)
        .counter("peak_working_set_bytes", peak)
        .counter("baseline_working_set_bytes", baseline_peak)
        .metric("elapsed_s", elapsed.as_secs_f64())
        .metric("mb_per_s", mb_s)
        .note(format!(
            "state {} coverage {}",
            outcome.state, outcome.coverage
        ));
    let record = timer.finish();
    println!("bench: {}", serde_json::to_string(&record)?);
    if let Some(p) = bench_out {
        use std::io::Write;
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)?;
        writeln!(f, "{}", serde_json::to_string(&record)?)?;
    }
    Ok(())
}

fn bench_search(
    data_dir: &std::path::Path,
    iterations: u32,
    concurrent_scan: Option<String>,
    bench_out: Option<PathBuf>,
    extra: Vec<String>,
    rebuild: bool,
) -> anyhow::Result<()> {
    let catalog = Catalog::open(data_dir.join("catalog.db"))?;
    let index = CatalogIndex::open(data_dir.join("index").join("catalog"))?;
    let content = ContentIndex::open(data_dir.join("index").join("content"))?;
    if rebuild {
        for s in catalog.list_sources()? {
            if s.published_generation.is_some() {
                let r = index.rebuild_source(&catalog, s.id)?;
                println!(
                    "rebuilt {} : {} docs in {:.0} ms",
                    s.name, r.documents, r.elapsed_ms
                );
            }
        }
    } else {
        let rebuilt = index.sync_sources(&catalog)?;
        for r in rebuilt {
            println!(
                "synced source {} : {} docs in {:.0} ms",
                r.source_id, r.documents, r.elapsed_ms
            );
        }
    }
    index.reload()?;
    let docs = index.num_docs();
    let mut timer = BenchTimer::start("query.catalog", data_dir.display().to_string());
    timer
        .counter("index_documents", docs)
        .counter("content_documents", content.num_docs())
        .counter("iterations", iterations as u64);

    // Optional concurrent rescan.
    let scan_handle = match &concurrent_scan {
        Some(name) => {
            let src = catalog
                .find_source_by_name(name)?
                .with_context(|| format!("no source named {name}"))?;
            let cat = catalog.clone();
            let sid = src.id;
            timer.note(format!("concurrent rescan of {name}"));
            Some(std::thread::spawn(move || {
                let lister = eidos_scanner::default_lister();
                let started = Instant::now();
                let r = run_scan(&cat, sid, lister.as_ref(), &RunScanOptions::default());
                (r.map(|s| s.stats.entries_seen), started.elapsed())
            }))
        }
        None => None,
    };

    let mut cases: Vec<(String, String, ResultMode, SortField)> = CASES
        .iter()
        .map(|c| (c.family.to_string(), c.text.to_string(), c.mode, c.sort))
        .collect();
    for q in &extra {
        cases.push((
            "custom".into(),
            q.clone(),
            ResultMode::Files,
            SortField::Relevance,
        ));
    }
    let mut per_family: BTreeMap<String, LatencySamples> = BTreeMap::new();
    let mut all = LatencySamples::default();
    let mut per_query: Vec<(String, f64, u64)> = Vec::new();
    let opts = ExecOptions::default();
    let mut errors = 0u64;
    for (family, text, mode, sort) in &cases {
        let parsed = match eidos_query::parse(text) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skip {text}: {e}");
                errors += 1;
                continue;
            }
        };
        let req = SearchRequest {
            mode: *mode,
            sort: Sort {
                field: *sort,
                descending: *sort != SortField::Name,
            },
            limit: 50,
            explain: false,
            snippets: false,
            ..SearchRequest::new(parsed.query)
        };
        let mut samples = LatencySamples::default();
        let mut total = 0u64;
        for _ in 0..iterations {
            let t = Instant::now();
            match search_with_content(&index, Some(&content), &catalog, &req, &opts) {
                Ok(r) => total = r.total.value,
                Err(e) => {
                    eprintln!("{text}: {e}");
                    errors += 1;
                    break;
                }
            }
            let d = t.elapsed();
            samples.push(d);
            all.push(d);
            per_family.entry(family.clone()).or_default().push(d);
        }
        per_query.push((text.clone(), samples.percentile_ms(95.0), total));
    }
    println!("{:<52} {:>10} {:>9}", "query", "p95 ms", "matches");
    for (q, p95, total) in &per_query {
        println!("{q:<52} {p95:>10.2} {total:>9}");
    }
    println!();
    for (family, s) in &per_family {
        println!(
            "{family:<10} p50 {:>7.2} ms  p95 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms  (n={})",
            s.percentile_ms(50.0),
            s.percentile_ms(95.0),
            s.percentile_ms(99.0),
            s.percentile_ms(100.0),
            s.len()
        );
        s.record_into(&mut timer, family);
    }
    all.record_into(&mut timer, "all");
    timer.counter("errors", errors);
    if let Some(h) = scan_handle {
        let (r, elapsed) = h.join().expect("scan thread");
        match r {
            Ok(entries) => {
                println!(
                    "concurrent rescan: {entries} entries in {:.1} s",
                    elapsed.as_secs_f64()
                );
                timer.metric("concurrent_scan_s", elapsed.as_secs_f64());
                timer.counter("concurrent_scan_entries", entries);
            }
            Err(e) => {
                println!("concurrent rescan failed: {e}");
                timer.note(format!("concurrent rescan failed: {e}"));
            }
        }
    }
    if errors > 0 {
        timer.fail();
    }
    let record = timer.finish();
    println!("bench: {}", serde_json::to_string(&record)?);
    if let Some(p) = bench_out {
        use std::io::Write;
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)?;
        writeln!(f, "{}", serde_json::to_string(&record)?)?;
    }
    Ok(())
}
