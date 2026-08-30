//! Standalone v0.5 service: catalog + scanner + change feeds + HTTP API in
//! one process.
//!
//! The in-process agent contract (scan a source, watch its change feed,
//! report completeness) lives behind [`state::AppState`] so that v1 can move
//! the scanner behind a transport without changing the API layer.

pub mod admission;
pub mod api;
#[cfg(test)]
mod api_contract;
mod api_json;
pub mod content_control;
pub mod content_preview;
pub mod content_workers;
pub mod export;
pub mod fleet_api;
pub mod follower;
#[cfg(target_os = "macos")]
mod fsevents_apply;
pub mod interactions_api;
pub mod retry_api;
pub mod scanner;
pub mod source_budget;
pub mod state;
#[cfg(windows)]
pub mod usn_apply;
pub mod watcher;
pub mod web;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

pub use web::WebAssets;

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Directory holding `catalog.db` and (later) search indexes.
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    /// Built web application directory on disk. Overrides the embedded UI.
    pub web_dir: Option<PathBuf>,
    /// Serve the web UI embedded in the executable when `web_dir` is unset.
    pub embedded_web: bool,
    /// Worker threads for enumeration.
    pub scan_threads: usize,
    /// Let the reconciler start periodic rescans of feed-less sources.
    pub auto_reconcile: bool,
    /// Run literal-text content extraction.
    pub content: bool,
    /// Extraction threads (global; per-source budgets apply on top). A
    /// durable operator override written by the workers control
    /// (`content-workers.json` in the data directory) wins over this value
    /// at startup.
    pub content_workers: usize,
    /// Bounds and deadlines for expensive HTTP operations.
    pub admission: admission::AdmissionConfig,
    /// Bounds on `/api/search/export`.
    pub export: export::ExportLimits,
    /// Start the fleet runtime (identity, sync listener/dialers as
    /// configured in `fleet/config.json`). Off only for tests that want a
    /// service without a network presence.
    pub fleet: bool,
}

impl ServiceConfig {
    /// Resolve the web UI source from `web_dir` / `embedded_web`.
    pub fn web_assets(&self) -> WebAssets {
        match (&self.web_dir, self.embedded_web) {
            (Some(dir), _) => WebAssets::Directory(dir.clone()),
            (None, true) => WebAssets::Embedded,
            (None, false) => WebAssets::Disabled,
        }
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            bind: "127.0.0.1:7700".parse().expect("static addr"),
            web_dir: None,
            embedded_web: true,
            scan_threads: 8,
            auto_reconcile: true,
            content: true,
            content_workers: 4,
            admission: admission::AdmissionConfig::default(),
            export: export::ExportLimits::default(),
            fleet: true,
        }
    }
}

/// Build state, start background watchers, and serve until Ctrl-C.
pub fn run(config: ServiceConfig) -> anyhow::Result<()> {
    run_with(config, |_| {}, async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

/// Build state, start background watchers, and serve until `shutdown`
/// resolves. `on_ready` is called once with the bound address, after the
/// listener accepts connections — a service host reports "running" there.
///
/// Returns once the HTTP server has drained. Background threads observe the
/// shutdown flag and stop on their own; durable state is crash-safe, so the
/// process may exit without joining them.
pub fn run_with<F>(
    config: ServiceConfig,
    on_ready: impl FnOnce(SocketAddr) + Send + 'static,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let state = Arc::new(state::AppState::open(&config)?);
    state.start_background()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let shutdown_state = state.clone();
    let web = config.web_assets();
    rt.block_on(async move {
        if config.fleet {
            // The fleet runtime is never a prerequisite for local readiness:
            // a failure here is reported and the service serves regardless.
            match eidos_fleet::Fleet::start(state.catalog.clone(), &config.data_dir) {
                Ok(fleet) => *state.fleet.lock() = Some(fleet),
                Err(e) => tracing::error!(error = %e, "fleet runtime did not start; local operation continues"),
            }
        }
        let app = api::router_with_web(state.clone(), &web);
        let listener = tokio::net::TcpListener::bind(config.bind).await?;
        let local = listener.local_addr().unwrap_or(config.bind);
        tracing::info!(bind = %local, data_dir = %config.data_dir.display(), "eidos service listening");
        on_ready(local);
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.await;
                tracing::info!("shutdown requested");
                shutdown_state.request_shutdown();
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;
    tracing::info!("eidos service stopped");
    Ok(())
}
