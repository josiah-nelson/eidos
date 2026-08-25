#[cfg(target_os = "macos")]
use clap::Parser;

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
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
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
