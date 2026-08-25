#[cfg(target_os = "macos")]
use clap::Parser;
#[cfg(target_os = "macos")]
use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt, Layer};

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
    tracing_subscriber::registry()
        .with(FseventDropLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .without_time()
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
