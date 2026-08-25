//! Read-only corpus profiler.
//!
//! Walks a tree with the platform lister, aggregates counts and bytes by
//! extension and file kind, records errors, and emits a machine-readable
//! `BenchRecord` plus an optional detailed JSON report. It never opens file
//! contents and never writes inside the profiled tree.

use anyhow::Context;
use clap::Args;
use eidos_domain::bench::{BenchRecord, BenchTimer};
use eidos_domain::{extension_of, FileKind, ObjectKind};
use eidos_scanner::{default_lister, walk, DirEvent, ScanErrorKind, WalkOptions};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct ProfileArgs {
    /// Root directory to profile (read-only).
    pub root: PathBuf,
    /// Worker threads for enumeration.
    #[arg(long, default_value_t = 8)]
    pub threads: usize,
    /// Maximum depth to descend (root = 0).
    #[arg(long)]
    pub max_depth: Option<u32>,
    /// How many extensions/directories/largest files to list.
    #[arg(long, default_value_t = 25)]
    pub top: usize,
    /// Append the benchmark record (one JSON line) to this file.
    #[arg(long)]
    pub bench_out: Option<PathBuf>,
    /// Write the full JSON report to this file.
    #[arg(long)]
    pub report_out: Option<PathBuf>,
    /// Label for the benchmark record target (defaults to the root path).
    #[arg(long)]
    pub label: Option<String>,
    /// Print the full JSON report to stdout instead of the summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Default, Serialize, Clone)]
struct ExtStat {
    count: u64,
    bytes: u64,
    allocated: u64,
}

#[derive(Serialize)]
struct Report {
    root: String,
    lister: &'static str,
    volume: Option<eidos_scanner::VolumeInfo>,
    elapsed_ms: f64,
    directories: u64,
    files: u64,
    reparse_points: u64,
    logical_bytes: u64,
    allocated_bytes: u64,
    allocated_known: bool,
    errors: u64,
    errors_by_kind: BTreeMap<String, u64>,
    error_samples: Vec<String>,
    lossy_names: u64,
    entries_per_sec: f64,
    text_candidate_files: u64,
    text_candidate_bytes: u64,
    by_kind: BTreeMap<String, ExtStat>,
    top_extensions_by_count: Vec<(String, ExtStat)>,
    top_extensions_by_bytes: Vec<(String, ExtStat)>,
    top_directories_by_bytes: Vec<(String, u64, u64)>,
    largest_files: Vec<(String, u64)>,
    max_depth_seen: u32,
    size_histogram: BTreeMap<String, u64>,
}

pub fn run(args: ProfileArgs) -> anyhow::Result<()> {
    let root = args
        .root
        .canonicalize()
        .unwrap_or_else(|_| args.root.clone());
    let label = args
        .label
        .clone()
        .unwrap_or_else(|| root.display().to_string());
    let lister = default_lister();
    let volume = match lister.volume_info(&root) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "volume info unavailable");
            None
        }
    };
    tracing::info!(root = %root.display(), lister = lister.name(), threads = args.threads, "profiling");

    let mut timer = BenchTimer::start("profile.walk", &label);
    let mut agg = Aggregator::new(args.top);
    let opts = WalkOptions {
        threads: args.threads,
        max_depth: args.max_depth,
        ..Default::default()
    };
    let stats = walk(&root, lister.as_ref(), &opts, |ev| agg.observe(ev));
    let elapsed_ms = stats.elapsed.as_secs_f64() * 1000.0;
    let entries = stats.entries;
    let eps = if elapsed_ms > 0.0 {
        entries as f64 / (elapsed_ms / 1000.0)
    } else {
        0.0
    };

    let report = agg.finish(&root, lister.name(), volume, elapsed_ms, eps);
    timer
        .counter("directories", report.directories)
        .counter("files", report.files)
        .counter("reparse_points", report.reparse_points)
        .counter("logical_bytes", report.logical_bytes)
        .counter("allocated_bytes", report.allocated_bytes)
        .counter("errors", report.errors)
        .counter("text_candidate_files", report.text_candidate_files)
        .counter("text_candidate_bytes", report.text_candidate_bytes)
        .metric("entries_per_sec", eps)
        .metric("walk_ms", elapsed_ms);
    if !report.allocated_known {
        timer.note("allocated sizes unavailable from lister");
    }
    let record = timer.finish();

    if let Some(p) = &args.bench_out {
        append_jsonl(p, &record)?;
    }
    if let Some(p) = &args.report_out {
        std::fs::write(p, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("writing {}", p.display()))?;
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_summary(&report, &record);
    }
    Ok(())
}

fn append_jsonl(path: &Path, record: &BenchRecord) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

struct DirAgg {
    path: String,
    parent: Option<eidos_scanner::DirToken>,
    own_bytes: u64,
    own_alloc: u64,
}

struct Aggregator {
    top: usize,
    dirs: HashMap<eidos_scanner::DirToken, DirAgg>,
    directories: u64,
    files: u64,
    reparse: u64,
    logical: u64,
    allocated: u64,
    allocated_known: bool,
    errors: u64,
    errors_by_kind: BTreeMap<String, u64>,
    error_samples: Vec<String>,
    lossy_names: u64,
    by_ext: HashMap<String, ExtStat>,
    largest: Vec<(String, u64)>,
    max_depth: u32,
    size_hist: BTreeMap<String, u64>,
}

impl Aggregator {
    fn new(top: usize) -> Self {
        Self {
            top,
            dirs: HashMap::new(),
            directories: 0,
            files: 0,
            reparse: 0,
            logical: 0,
            allocated: 0,
            allocated_known: true,
            errors: 0,
            errors_by_kind: BTreeMap::new(),
            error_samples: Vec::new(),
            lossy_names: 0,
            by_ext: HashMap::new(),
            largest: Vec::new(),
            max_depth: 0,
            size_hist: BTreeMap::new(),
        }
    }

    fn observe(&mut self, ev: DirEvent) {
        self.max_depth = self.max_depth.max(ev.depth);
        let path_str = display_path(&ev.path);
        let mut own_bytes = 0u64;
        let mut own_alloc = 0u64;
        match &ev.result {
            Ok(entries) => {
                self.directories += 1;
                for e in entries {
                    if e.name_lossy {
                        self.lossy_names += 1;
                    }
                    match e.kind {
                        ObjectKind::Directory => {}
                        ObjectKind::Reparse => self.reparse += 1,
                        _ => {
                            self.files += 1;
                            self.logical += e.size;
                            let alloc = match e.allocated {
                                Some(a) => a,
                                None => {
                                    self.allocated_known = false;
                                    e.size
                                }
                            };
                            self.allocated += alloc;
                            own_bytes += e.size;
                            own_alloc += alloc;
                            let ext = extension_of(&e.name);
                            let st = self.by_ext.entry(ext).or_default();
                            st.count += 1;
                            st.bytes += e.size;
                            st.allocated += alloc;
                            *self
                                .size_hist
                                .entry(size_bucket(e.size).to_string())
                                .or_default() += 1;
                            if self.largest.len() < self.top
                                || self.largest.last().is_some_and(|(_, s)| e.size > *s)
                            {
                                self.largest.push((
                                    format!("{path_str}{}{}", std::path::MAIN_SEPARATOR, e.name),
                                    e.size,
                                ));
                                self.largest.sort_by_key(|x| std::cmp::Reverse(x.1));
                                self.largest.truncate(self.top);
                            }
                        }
                    }
                }
            }
            Err(err) => {
                self.errors += 1;
                *self
                    .errors_by_kind
                    .entry(format!("{:?}", err.kind))
                    .or_default() += 1;
                if self.error_samples.len() < 20 {
                    self.error_samples
                        .push(format!("{} [{}]", err.path.display(), err.code));
                }
                if err.kind == ScanErrorKind::AccessDenied {
                    tracing::debug!(path = %err.path.display(), "access denied");
                }
            }
        }
        self.dirs.insert(
            ev.token,
            DirAgg {
                path: path_str,
                parent: ev.parent,
                own_bytes,
                own_alloc,
            },
        );
    }

    fn finish(
        self,
        root: &Path,
        lister: &'static str,
        volume: Option<eidos_scanner::VolumeInfo>,
        elapsed_ms: f64,
        eps: f64,
    ) -> Report {
        // Bottom-up subtree bytes: tokens are assigned in discovery order, so
        // a child's token is always greater than its parent's. Iterating in
        // descending token order accumulates children before parents.
        let mut subtree: HashMap<eidos_scanner::DirToken, (u64, u64)> = self
            .dirs
            .iter()
            .map(|(t, d)| (*t, (d.own_bytes, d.own_alloc)))
            .collect();
        let mut tokens: Vec<_> = self.dirs.keys().copied().collect();
        tokens.sort_by(|a, b| b.cmp(a));
        for t in &tokens {
            if let Some(parent) = self.dirs[t].parent {
                let (b, a) = subtree[t];
                if let Some(p) = subtree.get_mut(&parent) {
                    p.0 += b;
                    p.1 += a;
                }
            }
        }
        let mut top_dirs: Vec<(String, u64, u64)> = self
            .dirs
            .iter()
            .map(|(t, d)| (d.path.clone(), subtree[t].0, subtree[t].1))
            .collect();
        top_dirs.sort_by_key(|x| std::cmp::Reverse(x.1));
        top_dirs.truncate(self.top);

        let mut by_count: Vec<(String, ExtStat)> = self
            .by_ext
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        by_count.sort_by_key(|x| std::cmp::Reverse(x.1.count));
        by_count.truncate(self.top);
        let mut by_bytes: Vec<(String, ExtStat)> = self
            .by_ext
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        by_bytes.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
        by_bytes.truncate(self.top);

        let mut by_kind: BTreeMap<String, ExtStat> = BTreeMap::new();
        let mut text_files = 0;
        let mut text_bytes = 0;
        for (ext, st) in &self.by_ext {
            let kind = FileKind::from_extension(ext);
            let k = by_kind.entry(kind.as_str().to_string()).or_default();
            k.count += st.count;
            k.bytes += st.bytes;
            k.allocated += st.allocated;
            if kind.is_text_candidate() {
                text_files += st.count;
                text_bytes += st.bytes;
            }
        }

        Report {
            root: display_path(root),
            lister,
            volume,
            elapsed_ms,
            directories: self.directories,
            files: self.files,
            reparse_points: self.reparse,
            logical_bytes: self.logical,
            allocated_bytes: self.allocated,
            allocated_known: self.allocated_known,
            errors: self.errors,
            errors_by_kind: self.errors_by_kind,
            error_samples: self.error_samples,
            lossy_names: self.lossy_names,
            entries_per_sec: eps,
            text_candidate_files: text_files,
            text_candidate_bytes: text_bytes,
            by_kind,
            top_extensions_by_count: by_count,
            top_extensions_by_bytes: by_bytes,
            top_directories_by_bytes: top_dirs,
            largest_files: self.largest,
            max_depth_seen: self.max_depth,
            size_histogram: self.size_hist,
        }
    }
}

/// Strip the `\?\` extended-length prefix for human-readable output.
fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s
}

fn size_bucket(size: u64) -> &'static str {
    match size {
        0 => "0",
        1..=4_095 => "<4K",
        4_096..=65_535 => "4K-64K",
        65_536..=1_048_575 => "64K-1M",
        1_048_576..=16_777_215 => "1M-16M",
        16_777_216..=268_435_455 => "16M-256M",
        268_435_456..=1_073_741_823 => "256M-1G",
        _ => ">=1G",
    }
}

fn ext_label(e: &str) -> String {
    if e.is_empty() {
        "(none)".to_string()
    } else {
        format!(".{e}")
    }
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

fn print_summary(r: &Report, rec: &BenchRecord) {
    println!("Profile of {}  (lister: {})", r.root, r.lister);
    if let Some(v) = &r.volume {
        println!(
            "  volume: {} [{}] serial={:x} type={:?} feed={} case={} file_ids={} cluster={}",
            v.volume_root,
            v.filesystem,
            v.volume_serial,
            v.drive_type,
            v.native_feed.as_str(),
            match v.case_sensitive {
                Some(true) => "sensitive",
                Some(false) => "insensitive",
                None => "unknown",
            },
            v.supports_file_ids,
            v.bytes_per_cluster
        );
    }
    println!(
        "  {} dirs, {} files, {} reparse, {} errors, {} lossy names, max depth {}",
        r.directories, r.files, r.reparse_points, r.errors, r.lossy_names, r.max_depth_seen
    );
    println!(
        "  logical {}  allocated {}{}",
        human_bytes(r.logical_bytes),
        human_bytes(r.allocated_bytes),
        if r.allocated_known {
            ""
        } else {
            " (estimated)"
        }
    );
    println!(
        "  text candidates: {} files, {}",
        r.text_candidate_files,
        human_bytes(r.text_candidate_bytes)
    );
    println!(
        "  elapsed {:.1} ms, {:.0} entries/s",
        r.elapsed_ms, r.entries_per_sec
    );
    if !r.errors_by_kind.is_empty() {
        println!("  errors by kind: {:?}", r.errors_by_kind);
        for s in r.error_samples.iter().take(5) {
            println!("    {s}");
        }
    }
    println!("  by kind:");
    for (k, v) in &r.by_kind {
        println!(
            "    {k:<11} {:>9} files  {:>12}",
            v.count,
            human_bytes(v.bytes)
        );
    }
    println!("  top extensions by count:");
    for (e, s) in &r.top_extensions_by_count {
        println!(
            "    {:<13} {:>9}  {:>12}",
            ext_label(e),
            s.count,
            human_bytes(s.bytes)
        );
    }
    println!("  top extensions by bytes:");
    for (e, s) in &r.top_extensions_by_bytes {
        println!(
            "    {:<13} {:>9}  {:>12}",
            ext_label(e),
            s.count,
            human_bytes(s.bytes)
        );
    }
    println!("  top directories by subtree bytes:");
    for (p, b, a) in &r.top_directories_by_bytes {
        println!(
            "    {:>12} (alloc {:>12})  {}",
            human_bytes(*b),
            human_bytes(*a),
            p
        );
    }
    println!("  largest files:");
    for (p, b) in &r.largest_files {
        println!("    {:>12}  {}", human_bytes(*b), p);
    }
    println!("  size histogram: {:?}", r.size_histogram);
    println!("bench: {}", serde_json::to_string(rec).unwrap_or_default());
}
