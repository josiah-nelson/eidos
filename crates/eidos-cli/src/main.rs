//! `eidos` command-line entry point.
//!
//! - `profile`: read-only corpus profiler (Milestone 0)
//! - `source add|list|scan`: in-process catalog administration (Milestone 1)
//! - `serve`: HTTP API + web UI (Milestone 1+)
//! - `service install|start|stop|status|uninstall`: Windows service (M5)
//!
//! Search commands (Milestone 3) talk to the running service so the CLI and
//! web UI share one query contract.

/// Chunk verification allocates tens of kilobytes per fetched chunk across
/// eight threads; the system allocator serialises those on Windows.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod activity;
mod admin;
mod archive;
mod bench;
mod content;
mod detach;
mod fleet;
mod logging;
mod observe;
mod profile;
mod search;
#[cfg(windows)]
#[path = "service.rs"]
mod service;
#[cfg(target_os = "macos")]
#[path = "service_launchd.rs"]
mod service;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "eidos", version, about = "Filesystem indexer")]
struct Cli {
    /// Log filter (e.g. `info`, `eidos_scanner=debug`).
    #[arg(long, env = "EIDOS_LOG", default_value = "info", global = true)]
    log: String,
    /// Emit logs as JSON lines.
    #[arg(long, global = true)]
    log_json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read-only metadata profile of a directory tree (never modifies sources).
    Profile(profile::ProfileArgs),
    /// Manage catalog sources in-process.
    Source(admin::SourceArgs),
    /// Run the service in the foreground (HTTP API and web UI).
    Serve(ServeArgs),
    /// Install, control, or run eidos as a background service.
    #[cfg(any(windows, target_os = "macos"))]
    Service(service::ServiceArgs),
    /// Search through the running service (same API and schema as the web UI).
    Search(search::SearchArgs),
    /// Benchmarks over an existing data directory.
    Bench(bench::BenchArgs),
    /// Indexing activity of the running service (queues, workers, throughput).
    Activity(activity::ActivityArgs),
    /// Archive manifests from the running service (ZIP member inventories).
    Archive(archive::ArchiveArgs),
    /// Content job controls in the running service (retry failures).
    Content(content::ContentArgs),
    /// Manage a bounded, privacy-preserving workload observation study.
    Observe(observe::ObserveArgs),
    /// Fleet identity, enrollment, central role, and sync status.
    Fleet(fleet::FleetArgs),
}

/// Everything that configures a running service. Shared by `serve`
/// (foreground) and `service install|run` (the same flags are stored on the
/// service's command line).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ServeArgs {
    /// Data directory holding catalog.db and indexes.
    #[arg(long, env = "EIDOS_DATA_DIR", default_value = "data")]
    pub data_dir: PathBuf,
    /// Listen address. Keep on loopback unless the network is trusted.
    #[arg(long, env = "EIDOS_BIND", default_value = "127.0.0.1:7700")]
    pub bind: std::net::SocketAddr,
    /// Serve the web UI from this directory instead of the copy embedded in
    /// the executable.
    #[arg(long, env = "EIDOS_WEB_DIR", conflicts_with = "no_web")]
    pub web_dir: Option<PathBuf>,
    /// API only: serve no web UI.
    #[arg(long)]
    pub no_web: bool,
    /// Write logs to daily files in this directory (in addition to stderr
    /// when there is a console). The service always logs here.
    #[arg(long, env = "EIDOS_LOG_DIR")]
    pub log_dir: Option<PathBuf>,
    /// Start the service in the background (no console window) and return
    /// once it answers; does nothing if it is already running there.
    #[arg(long)]
    pub detach: bool,
    /// Enumeration worker threads per scan.
    #[arg(long, default_value_t = 8)]
    pub scan_threads: usize,
    /// Disable automatic periodic rescans of sources without a change feed.
    #[arg(long)]
    pub no_auto_reconcile: bool,
    /// Do not extract or index file content (metadata only).
    #[arg(long)]
    pub no_content: bool,
    /// Content extraction threads (per-source budgets apply on top).
    #[arg(long, env = "EIDOS_CONTENT_WORKERS", default_value_t = 4)]
    pub content_workers: usize,
    /// Expensive API operations (search, browse, counts) admitted at once.
    #[arg(long, env = "EIDOS_MAX_CONCURRENT_QUERIES", default_value_t = 4)]
    pub max_concurrent_queries: usize,
    /// Requests allowed to wait for a free slot before load is shed (503).
    #[arg(long, env = "EIDOS_QUERY_QUEUE_DEPTH", default_value_t = 32)]
    pub query_queue_depth: usize,
    /// How long a queued request waits for a free slot before it is shed.
    #[arg(long, env = "EIDOS_QUERY_QUEUE_WAIT_MS", default_value_t = 5_000)]
    pub query_queue_wait_ms: u64,
    /// Response deadline for search (the query itself runs to completion).
    #[arg(long, env = "EIDOS_SEARCH_TIMEOUT_MS", default_value_t = 30_000)]
    pub search_timeout_ms: u64,
    /// Response deadline for the other expensive operations.
    #[arg(long, env = "EIDOS_OPERATION_TIMEOUT_MS", default_value_t = 60_000)]
    pub operation_timeout_ms: u64,
    /// Maximum accepted JSON request body.
    #[arg(long, env = "EIDOS_MAX_BODY_BYTES", default_value_t = 1 << 20)]
    pub max_body_bytes: usize,
    /// Rows fetched per cursor step while streaming `/api/search/export`.
    #[arg(long, env = "EIDOS_EXPORT_PAGE_SIZE", default_value_t = 500)]
    pub export_page_size: u32,
    /// Hard cap on rows a single export may emit.
    #[arg(long, env = "EIDOS_EXPORT_MAX_ROWS", default_value_t = 100_000)]
    pub export_max_rows: u64,
    /// Exports allowed to stream at once (kept below --max-concurrent-queries).
    #[arg(long, env = "EIDOS_EXPORT_CONCURRENCY", default_value_t = 2)]
    pub export_concurrency: usize,
    /// Do not start the fleet runtime (no identity, listener, or dialers).
    #[arg(long)]
    pub no_fleet: bool,
}

impl ServeArgs {
    /// Normalise the directory arguments: drop `.` components and trailing
    /// separators. Windows Installer directory properties end in a
    /// backslash, which would escape a closing quote on a command line, so
    /// the installer writes `"[DIR]."`; this makes that spelling equal to
    /// the plain path everywhere the value is shown or joined.
    pub fn normalized(mut self) -> Self {
        fn clean(p: &std::path::Path) -> PathBuf {
            p.components().collect()
        }
        self.data_dir = clean(&self.data_dir);
        self.log_dir = self.log_dir.as_deref().map(clean);
        self.web_dir = self.web_dir.as_deref().map(clean);
        self
    }

    pub fn service_config(&self) -> eidos_service::ServiceConfig {
        eidos_service::ServiceConfig {
            data_dir: self.data_dir.clone(),
            bind: self.bind,
            web_dir: self.web_dir.clone(),
            embedded_web: !self.no_web,
            scan_threads: self.scan_threads,
            auto_reconcile: !self.no_auto_reconcile,
            content: !self.no_content,
            content_workers: self.content_workers,
            admission: eidos_service::admission::AdmissionConfig {
                concurrency: self.max_concurrent_queries.max(1),
                queue_depth: self.query_queue_depth,
                queue_wait: Duration::from_millis(self.query_queue_wait_ms),
                search_timeout: Duration::from_millis(self.search_timeout_ms),
                operation_timeout: Duration::from_millis(self.operation_timeout_ms),
                max_body_bytes: self.max_body_bytes,
            },
            export: eidos_service::export::ExportLimits {
                page_size: self.export_page_size.clamp(1, 1000),
                max_rows: self.export_max_rows,
                concurrency: self.export_concurrency,
            },
            fleet: !self.no_fleet,
        }
    }

    /// Render these arguments back into a command line (`--flag value`
    /// pairs) so they can be stored verbatim on a service registration.
    /// Every field is emitted explicitly: the service must not depend on the
    /// environment or defaults of a future version.
    pub fn to_command_line(&self) -> Vec<std::ffi::OsString> {
        let mut v: Vec<std::ffi::OsString> = Vec::new();
        let mut kv = |k: &str, val: std::ffi::OsString| {
            v.push(k.into());
            v.push(val);
        };
        kv("--data-dir", self.data_dir.clone().into());
        kv("--bind", self.bind.to_string().into());
        if let Some(dir) = &self.web_dir {
            kv("--web-dir", dir.clone().into());
        }
        if let Some(dir) = &self.log_dir {
            kv("--log-dir", dir.clone().into());
        }
        kv("--scan-threads", self.scan_threads.to_string().into());
        kv("--content-workers", self.content_workers.to_string().into());
        kv(
            "--max-concurrent-queries",
            self.max_concurrent_queries.to_string().into(),
        );
        kv(
            "--query-queue-depth",
            self.query_queue_depth.to_string().into(),
        );
        kv(
            "--query-queue-wait-ms",
            self.query_queue_wait_ms.to_string().into(),
        );
        kv(
            "--search-timeout-ms",
            self.search_timeout_ms.to_string().into(),
        );
        kv(
            "--operation-timeout-ms",
            self.operation_timeout_ms.to_string().into(),
        );
        kv("--max-body-bytes", self.max_body_bytes.to_string().into());
        kv(
            "--export-page-size",
            self.export_page_size.to_string().into(),
        );
        kv("--export-max-rows", self.export_max_rows.to_string().into());
        kv(
            "--export-concurrency",
            self.export_concurrency.to_string().into(),
        );
        if self.no_web {
            v.push("--no-web".into());
        }
        if self.no_auto_reconcile {
            v.push("--no-auto-reconcile".into());
        }
        if self.no_content {
            v.push("--no-content".into());
        }
        if self.no_fleet {
            v.push("--no-fleet".into());
        }
        if self.detach {
            v.push("--detach".into());
        }
        v
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The service host initialises its own logging once its supervisor has
    // handed it the start request.
    #[cfg(any(windows, target_os = "macos"))]
    if let Command::Service(args) = &cli.command {
        if args.is_run() {
            return service::run(args.clone(), cli.log.clone(), cli.log_json);
        }
    }
    // The Windows collector service likewise logs to its own data directory.
    if let Command::Observe(args) = &cli.command {
        if args.is_service_entry() {
            let Command::Observe(args) = cli.command else {
                unreachable!()
            };
            return observe::run(args, &cli.log);
        }
    }
    let _log_guard = match &cli.command {
        Command::Serve(args) => {
            logging::init(&cli.log, cli.log_json, args.log_dir.as_deref(), true)?
        }
        _ => logging::init(&cli.log, cli.log_json, None, true)?,
    };
    match cli.command {
        Command::Profile(args) => profile::run(args),
        Command::Source(args) => admin::run(args),
        Command::Bench(args) => bench::run(args),
        Command::Activity(args) => activity::run(args),
        Command::Archive(args) => archive::run(args),
        Command::Content(args) => content::run(args),
        Command::Observe(args) => observe::run(args, &cli.log),
        Command::Fleet(args) => fleet::run(args),
        #[cfg(any(windows, target_os = "macos"))]
        Command::Service(args) => service::run(args, cli.log, cli.log_json),
        Command::Search(args) => {
            let code = search::run(args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Serve(args) => {
            let args = args.normalized();
            warn_if_exposed(args.bind);
            if args.detach {
                return detach::start(&args);
            }
            eidos_service::run(args.service_config())
        }
    }
}

fn warn_if_exposed(bind: std::net::SocketAddr) {
    if !bind.ip().is_loopback() {
        tracing::warn!(bind = %bind, "binding beyond loopback: the API has no authentication in v0.5");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ServeArgs {
        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            serve: ServeArgs,
        }
        Wrap::try_parse_from(std::iter::once("x").chain(args.iter().copied()))
            .expect("parse")
            .serve
    }

    #[test]
    fn command_line_round_trips_every_field() {
        let original = parse(&[
            "--data-dir",
            r"C:\ProgramData\eidos data",
            "--bind",
            "127.0.0.1:7711",
            "--web-dir",
            r"C:\Program Files\eidos\web",
            "--log-dir",
            r"C:\ProgramData\eidos\logs",
            "--scan-threads",
            "3",
            "--no-auto-reconcile",
            "--no-content",
            "--content-workers",
            "2",
            "--max-concurrent-queries",
            "5",
            "--query-queue-depth",
            "7",
            "--query-queue-wait-ms",
            "11",
            "--search-timeout-ms",
            "13",
            "--operation-timeout-ms",
            "17",
            "--max-body-bytes",
            "19",
            "--export-page-size",
            "23",
            "--export-max-rows",
            "29",
            "--export-concurrency",
            "31",
        ]);
        let line = original.to_command_line();
        let strs: Vec<String> = line
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let refs: Vec<&str> = strs.iter().map(String::as_str).collect();
        assert_eq!(parse(&refs), original);
        // Defaults survive too, and `--no-web` is carried.
        let plain = parse(&["--no-web"]);
        let strs: Vec<String> = plain
            .to_command_line()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let refs: Vec<&str> = strs.iter().map(String::as_str).collect();
        assert_eq!(parse(&refs), plain);
        assert!(refs.contains(&"--no-web"));
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_installer_style_directories() {
        let args = parse(&[
            "--data-dir",
            r"C:\ProgramData\eidos\.",
            "--log-dir",
            r"C:\ProgramData\eidos\logs\.",
        ])
        .normalized();
        assert_eq!(args.data_dir, PathBuf::from(r"C:\ProgramData\eidos"));
        assert_eq!(
            args.log_dir,
            Some(PathBuf::from(r"C:\ProgramData\eidos\logs"))
        );
        assert_eq!(args.web_dir, None);
    }
}
