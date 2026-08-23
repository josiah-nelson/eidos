//! Standalone v0.5 service: catalog + scanner + change feeds + HTTP API in
//! one process.
//!
//! The in-process agent contract (scan a source, watch its change feed,
//! report completeness) lives behind [`state::AppState`] so that v1 can move
//! the scanner behind a transport without changing the API layer.

pub mod admission;
pub mod api;
pub mod content_preview;
pub mod content_workers;
pub mod export;
pub mod follower;
pub mod retry_api;
pub mod scanner;
pub mod source_budget;
pub mod state;
#[cfg(windows)]
pub mod usn_apply;
pub mod watcher;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Directory holding `catalog.db` and (later) search indexes.
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    /// Built web application directory (`web/dist`). Optional.
    pub web_dir: Option<PathBuf>,
    /// Worker threads for enumeration.
    pub scan_threads: usize,
    /// Let the reconciler start periodic rescans of feed-less sources.
    pub auto_reconcile: bool,
    /// Run literal-text content extraction.
    pub content: bool,
    /// Extraction threads (global; per-source budgets apply on top).
    pub content_workers: usize,
    /// Bounds and deadlines for expensive HTTP operations.
    pub admission: admission::AdmissionConfig,
    /// Bounds on `/api/search/export`.
    pub export: export::ExportLimits,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            bind: "127.0.0.1:7700".parse().expect("static addr"),
            web_dir: Some(PathBuf::from("web/dist")),
            scan_threads: 8,
            auto_reconcile: true,
            content: true,
            content_workers: 4,
            admission: admission::AdmissionConfig::default(),
            export: export::ExportLimits::default(),
        }
    }
}

/// Build state, start background watchers, and serve until Ctrl-C.
pub fn run(config: ServiceConfig) -> anyhow::Result<()> {
    let state = Arc::new(state::AppState::open(&config)?);
    state.start_background()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let shutdown_state = state.clone();
    rt.block_on(async move {
        let app = api::router(state.clone(), config.web_dir.as_deref());
        let listener = tokio::net::TcpListener::bind(config.bind).await?;
        tracing::info!(bind = %config.bind, data_dir = %config.data_dir.display(), "eidos service listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("shutdown requested");
                shutdown_state.request_shutdown();
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}
