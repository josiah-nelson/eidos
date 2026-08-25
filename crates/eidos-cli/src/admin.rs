//! In-process administrative commands (source add/list/scan).
//!
//! These open the catalog directly so the G:/R: metadata import benchmark
//! can run without the HTTP service. Search commands (Milestone 3) go over
//! the HTTP API so the CLI and UI share one query contract.

use anyhow::Context;
use clap::{Args, Subcommand};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::bench::BenchTimer;
use eidos_domain::SourceKind;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct SourceArgs {
    /// Data directory holding catalog.db.
    #[arg(long, env = "EIDOS_DATA_DIR", default_value = "data", global = true)]
    pub data_dir: PathBuf,
    #[command(subcommand)]
    pub command: SourceCommand,
}

#[derive(Subcommand, Debug)]
pub enum SourceCommand {
    /// Register a source root (does not scan).
    Add {
        name: String,
        root: PathBuf,
        /// Force a source kind; auto-detected from the volume by default.
        #[arg(long)]
        kind: Option<String>,
    },
    /// List sources with counts and completeness.
    List,
    /// Run a full read-only metadata scan of a source into the catalog.
    Scan {
        name: String,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        /// Append a benchmark record to this JSONL file.
        #[arg(long)]
        bench_out: Option<PathBuf>,
    },
    /// Set the content policy of a source (extraction on/off, concurrency).
    Content {
        name: String,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        /// Concurrent content jobs for this source (1 for HDDs/SMB).
        #[arg(long)]
        concurrency: Option<u32>,
    },
}

pub fn run(args: SourceArgs) -> anyhow::Result<()> {
    let catalog = Catalog::open(args.data_dir.join("catalog.db"))?;
    let report = catalog.recover()?;
    for (sid, gen) in &report.aborted_generations {
        tracing::warn!(
            source = sid.0,
            generation = gen,
            "aborted interrupted scan generation"
        );
    }
    let host_name = eidos_domain::bench::hostname();
    let host = catalog.ensure_host(&host_name, std::env::consts::OS)?;
    let lister = eidos_scanner::default_lister();
    match args.command {
        SourceCommand::Add { name, root, kind } => {
            let root = PathBuf::from(eidos_scanner::normalize_root(&root.display().to_string()));
            anyhow::ensure!(
                root.is_dir(),
                "{} is not an accessible directory",
                root.display()
            );
            let kind = match kind {
                Some(k) => k.parse::<SourceKind>().context("invalid kind")?,
                None => match lister.volume_info(&root) {
                    Ok(v) => v.source_kind(),
                    Err(_) => eidos_scanner::GENERIC_SOURCE_KIND,
                },
            };
            let id = catalog.add_source(&NewSource {
                host_id: host,
                name: name.clone(),
                kind,
                root_path: root.display().to_string(),
                aliases: vec![],
            })?;
            if let Ok(v) = lister.volume_info(&root) {
                catalog.upsert_volume(host, id, &v)?;
            }
            println!("added source {id} '{name}' ({kind}) at {}", root.display());
        }
        SourceCommand::List => {
            for s in catalog.list_sources()? {
                let counts = catalog.source_counts(s.id)?;
                let listing_errors = catalog.published_listing_errors(s.id)?;
                let c = eidos_catalog::read::completeness_from(&s, &counts, listing_errors);
                println!(
                    "{:<4} {:<16} {:<16} {:<18} gen={:<3} files={:<8} dirs={:<6} logical={:<12} alloc={:<12} pending={} excluded={} errors={} meta_complete={} {}",
                    s.id.0,
                    s.name,
                    s.kind,
                    s.state,
                    s.published_generation.map_or("-".to_string(), |g| g.to_string()),
                    counts.files,
                    counts.directories,
                    crate::profile::human_bytes(counts.logical_bytes),
                    crate::profile::human_bytes(counts.allocated_bytes),
                    counts.content_pending,
                    counts.content_excluded,
                    counts.open_errors,
                    c.metadata_complete,
                    s.root_path
                );
            }
        }
        SourceCommand::Content {
            name,
            enable,
            disable,
            concurrency,
        } => {
            let s = catalog
                .find_source_by_name(&name)?
                .with_context(|| format!("no source named '{name}'"))?;
            let enabled = if enable {
                true
            } else if disable {
                false
            } else {
                s.content_enabled
            };
            let concurrency = concurrency.unwrap_or(s.content_concurrency).clamp(1, 64);
            catalog.set_content_policy(s.id, enabled, concurrency)?;
            println!(
                "source {} '{}': content {} concurrency {}",
                s.id,
                s.name,
                if enabled { "enabled" } else { "disabled" },
                concurrency
            );
        }
        SourceCommand::Scan {
            name,
            threads,
            bench_out,
        } => {
            let s = catalog
                .find_source_by_name(&name)?
                .with_context(|| format!("no source named '{name}'"))?;
            if let Ok(v) = lister.volume_info(std::path::Path::new(&s.root_path)) {
                catalog.upsert_volume(host, s.id, &v)?;
            }
            let mut timer = BenchTimer::start("scan.generic", &name);
            let opts = RunScanOptions {
                walk: eidos_scanner::WalkOptions {
                    threads,
                    ..Default::default()
                },
                ..Default::default()
            };
            let summary = run_scan(&catalog, s.id, lister.as_ref(), &opts)?;
            let counts = catalog.source_counts(s.id)?;
            timer
                .counter("dirs_listed", summary.stats.dirs_listed)
                .counter("entries_seen", summary.stats.entries_seen)
                .counter("errors", summary.stats.errors)
                .counter("objects_created", summary.stats.objects_created)
                .counter("objects_updated", summary.stats.objects_updated)
                .counter(
                    "tombstoned",
                    summary.tombstoned_entries + summary.tombstoned_objects,
                )
                .counter("commits", summary.stats.commits)
                .counter("files", counts.files)
                .counter("directories", counts.directories)
                .counter("logical_bytes", counts.logical_bytes)
                .counter("allocated_bytes", counts.allocated_bytes)
                .metric("scan_ms", summary.elapsed_ms)
                .metric(
                    "entries_per_sec",
                    summary.stats.entries_seen as f64 / (summary.elapsed_ms / 1000.0).max(1e-9),
                );
            let db_bytes = std::fs::metadata(args.data_dir.join("catalog.db"))
                .map(|m| m.len())
                .unwrap_or(0);
            timer.counter("catalog_db_bytes", db_bytes);
            let record = timer.finish();
            println!("{}", serde_json::to_string_pretty(&summary)?);
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
        }
    }
    Ok(())
}
