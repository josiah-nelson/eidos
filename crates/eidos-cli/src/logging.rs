//! Tracing setup shared by the foreground commands and the service host.
//!
//! Logs go to stderr when there is a console and, when a log directory is
//! given, to daily-rotated files `eidos.<date>.log` in that directory. A
//! service has no console, so the file is its only output; it is created
//! before the SCM is told the service is running so a start failure is
//! always on disk somewhere an operator can find it.

use std::path::Path;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Keeps the non-blocking file writer alive; drop flushes it.
pub struct LogGuard(#[allow(dead_code)] Option<tracing_appender::non_blocking::WorkerGuard>);

/// File name prefix; the appender adds `.<date>`.
pub const FILE_PREFIX: &str = "eidos.log";

pub fn init(filter: &str, json: bool, log_dir: Option<&Path>, stderr: bool) -> LogGuard {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(env_filter);

    let (file_layer, guard) = match log_dir {
        Some(dir) => {
            // Creating the directory here (rather than failing later) means
            // the very first line the service writes is where `status` says
            // it is.
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("eidos: cannot create log directory {}: {e}", dir.display());
            }
            let appender = tracing_appender::rolling::daily(dir, FILE_PREFIX);
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
    LogGuard(guard)
}
