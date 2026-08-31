//! Tracing setup shared by the foreground commands and the service host.
//!
//! Logs go to stderr when there is a console and, when a log directory is
//! given, to daily-rotated files `eidos.log.<date>` in that directory. A
//! service has no console, so the file is its only output; the directory
//! and the first file are created here, up front, and a failure is an
//! error rather than a silent start without diagnostics.

use anyhow::Context;
use std::path::Path;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Keeps the non-blocking file writer alive; drop flushes it.
pub struct LogGuard(#[allow(dead_code)] Option<tracing_appender::non_blocking::WorkerGuard>);

/// Daily files kept on disk; the appender deletes older ones. Fleet nodes
/// run unattended for months, so retention is bounded here rather than by
/// an operator remembering to clean up.
const LOG_RETENTION_DAYS: usize = 14;

/// File name prefix; the appender adds `.<date>`.
pub const FILE_PREFIX: &str = "eidos.log";

pub fn init(
    filter: &str,
    json: bool,
    log_dir: Option<&Path>,
    stderr: bool,
) -> anyhow::Result<LogGuard> {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(env_filter);

    let (file_layer, guard) = match log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating log directory {}", dir.display()))?;
            // `build` opens today's file now, so an unwritable directory
            // fails here instead of dropping every later line.
            let appender = RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix(FILE_PREFIX)
                .max_log_files(LOG_RETENTION_DAYS)
                .build(dir)
                .with_context(|| format!("opening the log file in {}", dir.display()))?;
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (Some(writer), Some(guard))
        }
        None => (None, None),
    };

    let stderr_layer = stderr.then(|| {
        if json {
            fmt::layer().json().with_writer(std::io::stderr).boxed()
        } else {
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .boxed()
        }
    });
    let file_layer = file_layer.map(|w| {
        if json {
            fmt::layer().json().with_writer(w).with_ansi(false).boxed()
        } else {
            fmt::layer().with_writer(w).with_ansi(false).boxed()
        }
    });
    registry.with(stderr_layer).with(file_layer).init();
    Ok(LogGuard(guard))
}
