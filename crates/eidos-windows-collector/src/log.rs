//! Bounded log files under the data directory: daily rotation, seven files
//! kept. The service has no console, so this is its only diagnostic output;
//! the foreground runner also writes to stderr.

use std::path::Path;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub struct LogGuard(#[allow(dead_code)] tracing_appender::non_blocking::WorkerGuard);

pub fn init(data_dir: &Path, filter: &str, stderr: bool) -> anyhow::Result<LogGuard> {
    let dir = data_dir.join("logs");
    std::fs::create_dir_all(&dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("collector.log")
        .max_log_files(7)
        .build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer().with_writer(writer).with_ansi(false);
    let stderr_layer = stderr.then(|| fmt::layer().with_writer(std::io::stderr).with_target(false));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();
    Ok(LogGuard(guard))
}
