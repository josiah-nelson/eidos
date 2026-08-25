#[cfg(target_os = "macos")]
use clap::Parser;
#[cfg(target_os = "macos")]
use std::io::{Seek, Write};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use tracing_subscriber::{
    filter::filter_fn, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt, Layer,
};

#[cfg(target_os = "macos")]
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct BoundedLog {
    file: Arc<Mutex<std::fs::File>>,
    max_bytes: u64,
}

#[cfg(target_os = "macos")]
impl BoundedLog {
    fn open() -> std::io::Result<Self> {
        Self::open_at("/var/log/eidos-collector.log", MAX_LOG_BYTES)
    }

    fn open_at(path: impl AsRef<std::path::Path>, max_bytes: u64) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            max_bytes,
        })
    }
}

#[cfg(target_os = "macos")]
struct LogWriter {
    file: Arc<Mutex<std::fs::File>>,
    max_bytes: u64,
}

#[cfg(target_os = "macos")]
impl Write for LogWriter {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("log lock poisoned"))?;
        if file.metadata()?.len().saturating_add(input.len() as u64) > self.max_bytes {
            file.set_len(0)?;
            file.rewind()?;
        }
        file.write(input)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file
            .lock()
            .map_err(|_| std::io::Error::other("log lock poisoned"))?
            .flush()
    }
}

#[cfg(target_os = "macos")]
impl<'a> MakeWriter<'a> for BoundedLog {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            file: self.file.clone(),
            max_bytes: self.max_bytes,
        }
    }
}

#[cfg(target_os = "macos")]
struct FseventDropLayer;

#[cfg(target_os = "macos")]
impl<S: tracing::Subscriber> Layer<S> for FseventDropLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() == tracing::Level::ERROR
            && event.metadata().target().starts_with("fsevent_stream")
        {
            eidos_macos_collector::daemon::note_fsevent_binding_drop();
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Parser)]
#[command(name = "eidos-collector", version)]
struct Args {
    #[arg(long)]
    endpoint_security: bool,
    #[arg(long)]
    entitlement_claimed: bool,
    #[arg(long, default_value = eidos_macos_collector::DEFAULT_DATA_DIR)]
    data_dir: std::path::PathBuf,
    #[arg(long, default_value = eidos_macos_collector::DEFAULT_SOCKET)]
    socket: std::path::PathBuf,
    #[arg(long, default_value = eidos_macos_collector::DEFAULT_EXPORT_DIR)]
    export_dir: std::path::PathBuf,
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log = BoundedLog::open()?;
    tracing_subscriber::registry()
        .with(FseventDropLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .without_time()
                .with_writer(log)
                .with_filter(filter_fn(|metadata| {
                    !metadata.target().starts_with("fsevent_stream")
                })),
        )
        .init();
    let args = Args::parse();
    eidos_macos_collector::daemon::run(eidos_macos_collector::daemon::Config {
        endpoint_security: args.endpoint_security,
        entitlement_claimed: args.entitlement_claimed,
        data_dir: args.data_dir,
        socket: args.socket,
        export_dir: args.export_dir,
    })
    .await
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("eidos-collector is available only on macOS");
    std::process::exit(2);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{BoundedLog, MakeWriter, Write};

    #[test]
    fn log_is_truncated_before_crossing_its_bound() {
        let path = std::env::temp_dir().join(format!("eidos-bounded-log-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = BoundedLog::open_at(&path, 16).unwrap();
        log.make_writer().write_all(b"123456789012").unwrap();
        log.make_writer().write_all(b"abcdefgh").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdefgh");
        std::fs::remove_file(path).unwrap();
    }
}
