//! `eidos` command-line entry point.
//!
//! - `profile`: read-only corpus profiler (Milestone 0)
//! - `source add|list|scan`: in-process catalog administration (Milestone 1)
//! - `serve`: HTTP API + web UI (Milestone 1+)
//!
//! Search commands (Milestone 3) talk to the running service so the CLI and
//! web UI share one query contract.

mod admin;
mod bench;
mod profile;
mod search;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

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
    /// Run the service (HTTP API and web UI).
    Serve(ServeArgs),
    /// Search through the running service (same API and schema as the web UI).
    Search(search::SearchArgs),
    /// Benchmarks over an existing data directory.
    Bench(bench::BenchArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Data directory holding catalog.db and indexes.
    #[arg(long, env = "EIDOS_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// Listen address. Keep on loopback unless the network is trusted.
    #[arg(long, env = "EIDOS_BIND", default_value = "127.0.0.1:7700")]
    bind: std::net::SocketAddr,
    /// Built web UI directory (web/dist). Pass an empty string for API only.
    #[arg(long, env = "EIDOS_WEB_DIR", default_value = "web/dist")]
    web_dir: String,
    /// Enumeration worker threads per scan.
    #[arg(long, default_value_t = 8)]
    scan_threads: usize,
    /// Disable automatic periodic rescans of sources without a change feed.
    #[arg(long)]
    no_auto_reconcile: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log, cli.log_json);
    match cli.command {
        Command::Profile(args) => profile::run(args),
        Command::Source(args) => admin::run(args),
        Command::Bench(args) => bench::run(args),
        Command::Search(args) => {
            let code = search::run(args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Serve(args) => {
            if !args.bind.ip().is_loopback() {
                tracing::warn!(bind = %args.bind, "binding beyond loopback: the API has no authentication in v0.5");
            }
            eidos_service::run(eidos_service::ServiceConfig {
                data_dir: args.data_dir,
                bind: args.bind,
                web_dir: if args.web_dir.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(args.web_dir))
                },
                scan_threads: args.scan_threads,
                auto_reconcile: !args.no_auto_reconcile,
            })
        }
    }
}

fn init_tracing(filter: &str, json: bool) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(env_filter);
    if json {
        registry
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .init();
    } else {
        registry
            .with(fmt::layer().with_writer(std::io::stderr).with_target(false))
            .init();
    }
}
