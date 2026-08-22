//! `eidos bench search` — in-process query latency benchmark against an
//! existing data directory, optionally while a rescan runs concurrently.
//! Emits an `eidos-bench/1` record with p50/p95/p99 per query family.

use anyhow::Context;
use clap::{Args, Subcommand};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::Catalog;
use eidos_domain::bench::{BenchTimer, LatencySamples};
use eidos_domain::{ResultMode, SearchRequest, Sort, SortField};
use eidos_search::exec::{search, ExecOptions};
use eidos_search::CatalogIndex;
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
        text: "clr metadata registry",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "name",
        text: "name:=Logfile.XML",
        mode: ResultMode::Files,
        sort: SortField::Relevance,
    },
    Case {
        family: "regex",
        text: "name:/^10\\.92\\.161\\.\\d+( \\(\\d+\\))?$/",
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
    }
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
            match search(&index, &catalog, &req, &opts) {
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
