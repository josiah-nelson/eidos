//! `eidos observe`: the workload observation study. `inspect` is portable;
//! the remaining commands talk to the platform collector — a root
//! LaunchDaemon over a Unix socket on macOS, a LocalSystem service over a
//! named pipe on Windows.

use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct ObserveArgs {
    #[command(subcommand)]
    command: ObserveCommand,
}

impl ObserveArgs {
    /// The Windows service entry point initialises its own logging.
    pub fn is_service_entry(&self) -> bool {
        matches!(
            self.command,
            ObserveCommand::Run(RunArgs { service: true, .. })
        )
    }
}

#[derive(Debug, Subcommand)]
enum ObserveCommand {
    /// Create the study key (login keychain on macOS; DPAPI machine scope
    /// on Windows) and the default configuration.
    Init(InitArgs),
    /// Windows: register the collector as a LocalSystem service.
    #[cfg(windows)]
    Install(InstallArgs),
    /// Windows: stop and remove the collector service (data is kept).
    #[cfg(windows)]
    Uninstall,
    /// Windows: start the collector service.
    #[cfg(windows)]
    Start,
    /// Windows: stop the collector service.
    #[cfg(windows)]
    Stop,
    /// Windows: switch lanes on or off at runtime.
    #[cfg(windows)]
    Lanes(LanesArgs),
    /// Windows: run one read-only enumeration probe now.
    #[cfg(windows)]
    Probe {
        /// Drive root such as `D:\`; all fixed volumes when omitted.
        volume: Option<String>,
    },
    /// Run the collector: the user-session key handoff on macOS; on
    /// Windows the collector itself, in the foreground unless --service.
    Run(RunArgs),
    /// Show collector capabilities, feed health, and ring usage.
    Status(SocketArgs),
    /// Add a keyed phase marker; the supplied label is never persisted.
    Mark {
        label: String,
        #[command(flatten)]
        socket: SocketArgs,
    },
    /// Ask the collector for a versioned study bundle and copy it locally.
    Export {
        #[arg(long, short, default_value = "observation.eidos-observation.zst")]
        output: PathBuf,
        #[command(flatten)]
        socket: SocketArgs,
    },
    /// List exactly the fields and record count in a study bundle.
    Inspect { bundle: PathBuf },
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Replace an existing study key, making prior object tokens unlinkable.
    #[arg(long)]
    force: bool,
    /// Windows: import a cohort-shared 32-byte key (64 hex characters) so
    /// content fingerprints compare across hosts. Generated when omitted.
    #[cfg(windows)]
    #[arg(long)]
    key_hex: Option<String>,
    #[cfg(windows)]
    #[command(flatten)]
    data: DataDirArgs,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Windows: service control manager entry point (not for interactive use).
    #[cfg(windows)]
    #[arg(long, hide = true)]
    pub service: bool,
    #[cfg(windows)]
    #[command(flatten)]
    pub data: DataDirArgs,
    #[cfg(not(windows))]
    #[arg(skip)]
    pub service: bool,
    #[cfg(target_os = "macos")]
    #[arg(long, default_value = eidos_macos_collector::DEFAULT_SOCKET)]
    socket: PathBuf,
}

#[cfg(windows)]
#[derive(Debug, Args, Clone)]
pub struct DataDirArgs {
    /// Collector data directory (spool, key, configuration, exports, logs).
    #[arg(long, env = "EIDOS_COLLECTOR_DIR", default_value = eidos_windows_collector::DEFAULT_DATA_DIR)]
    pub data_dir: PathBuf,
}

#[cfg(windows)]
#[derive(Debug, Args)]
pub struct InstallArgs {
    #[command(flatten)]
    data: DataDirArgs,
    /// Start the service once it is installed.
    #[arg(long)]
    start_now: bool,
}

#[cfg(windows)]
#[derive(Debug, Args)]
pub struct LanesArgs {
    #[arg(long, value_parser = parse_switch)]
    usn: Option<bool>,
    #[arg(long, value_parser = parse_switch)]
    etw: Option<bool>,
    #[arg(long, value_parser = parse_switch)]
    content: Option<bool>,
    #[arg(long, value_parser = parse_switch)]
    enumeration: Option<bool>,
}

#[cfg(windows)]
fn parse_switch(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("expected on or off, got {other:?}")),
    }
}

#[derive(Debug, Args)]
pub struct SocketArgs {
    #[cfg(target_os = "macos")]
    #[arg(long, default_value = eidos_macos_collector::DEFAULT_SOCKET)]
    socket: PathBuf,
}

pub fn run(args: ObserveArgs, log_filter: &str) -> anyhow::Result<()> {
    match args.command {
        ObserveCommand::Inspect { bundle } => inspect(&bundle),
        #[cfg(windows)]
        other => windows::run(other, log_filter),
        #[cfg(target_os = "macos")]
        other => macos::run(other, log_filter),
        #[cfg(not(any(windows, target_os = "macos")))]
        _ => {
            let _ = log_filter;
            anyhow::bail!("this observe command requires Windows or macOS")
        }
    }
}

fn inspect(bundle: &std::path::Path) -> anyhow::Result<()> {
    let inspection = eidos_observe::inspect_bundle(bundle)?;
    println!("schema: {}", inspection.schema);
    println!("records: {}", inspection.records);
    println!("fields:");
    for field in inspection.fields {
        println!("  {field}");
    }
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use eidos_windows_collector::client::{expect_accepted, request};
    use eidos_windows_collector::protocol::{Request, Response};
    use eidos_windows_collector::{keystore, service};

    pub fn run(command: ObserveCommand, log_filter: &str) -> anyhow::Result<()> {
        match command {
            ObserveCommand::Init(args) => init(args),
            ObserveCommand::Install(args) => service::install(&args.data.data_dir, args.start_now),
            ObserveCommand::Uninstall => service::uninstall(),
            ObserveCommand::Start => service::start(),
            ObserveCommand::Stop => service::stop(),
            ObserveCommand::Lanes(args) => expect_accepted(request(&Request::SetLanes {
                usn: args.usn,
                etw: args.etw,
                content: args.content,
                enumeration: args.enumeration,
            })?),
            ObserveCommand::Probe { volume } => {
                expect_accepted(request(&Request::Probe { volume })?)
            }
            ObserveCommand::Run(args) => {
                if args.service {
                    let _guard =
                        eidos_windows_collector::log::init(&args.data.data_dir, log_filter, false)?;
                    service::run_service(args.data.data_dir)
                } else {
                    foreground(args.data.data_dir)
                }
            }
            ObserveCommand::Status(_) => match request(&Request::Status)? {
                Response::Status { status } => {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                    Ok(())
                }
                Response::Error { message } => anyhow::bail!(message),
                other => anyhow::bail!("unexpected collector response: {other:?}"),
            },
            ObserveCommand::Mark { label, .. } => {
                expect_accepted(request(&Request::Mark { label })?)
            }
            ObserveCommand::Export { output, .. } => match request(&Request::Export)? {
                Response::Exported { staged_file } => {
                    let mut source = std::fs::File::open(&staged_file)?;
                    let mut destination = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&output)?;
                    std::io::copy(&mut source, &mut destination)?;
                    println!("{}", output.display());
                    Ok(())
                }
                Response::Error { message } => anyhow::bail!(message),
                other => anyhow::bail!("unexpected collector response: {other:?}"),
            },
            ObserveCommand::Inspect { .. } => unreachable!("handled by the portable path"),
        }
    }

    fn init(args: InitArgs) -> anyhow::Result<()> {
        let imported = match args.key_hex {
            Some(hex) => Some(parse_key(&hex)?),
            None => None,
        };
        let data_dir = &args.data.data_dir;
        let created = keystore::create(data_dir, imported, args.force)?;
        let config = eidos_windows_collector::config::CollectorConfig::load(data_dir)?;
        if !eidos_windows_collector::config::CollectorConfig::path(data_dir).exists() {
            config.save(data_dir)?;
        }
        if created {
            println!(
                "study key created under DPAPI machine scope in {}",
                data_dir.display()
            );
        } else {
            println!("study key already exists (use --force to replace it)");
        }
        match service::registration()? {
            Some(state) => println!("service {}: {state}", eidos_windows_collector::SERVICE_NAME),
            None => println!("service not installed; run `eidos observe install --start-now`"),
        }
        Ok(())
    }

    fn parse_key(hex: &str) -> anyhow::Result<[u8; 32]> {
        let hex = hex.trim();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("--key-hex must be exactly 64 hexadecimal characters");
        }
        let mut key = [0u8; 32];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)?;
        }
        Ok(key)
    }

    /// Foreground collector with Ctrl-C as the stop control; the same
    /// daemon the service runs, minus the SCM.
    fn foreground(data_dir: PathBuf) -> anyhow::Result<()> {
        let (tx, rx) = service::control_channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let stop = stop.clone();
            ctrlc_handler(move || {
                if !stop.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    let _ = tx.send(eidos_windows_collector::daemon::ControlEvent::Stop);
                }
            })?;
        }
        eidos_windows_collector::daemon::run(
            eidos_windows_collector::daemon::Options { data_dir },
            rx,
            || eprintln!("collector running in the foreground; Ctrl-C stops it"),
        )
    }

    fn ctrlc_handler(handler: impl Fn() + Send + 'static) -> anyhow::Result<()> {
        std::thread::Builder::new()
            .name("ctrl-c".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime for ctrl-c");
                runtime.block_on(async {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        handler();
                    }
                });
            })?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use eidos_macos_collector::protocol::{Request, Response};

    pub fn run(command: ObserveCommand, _log_filter: &str) -> anyhow::Result<()> {
        match command {
            ObserveCommand::Init(args) => {
                if eidos_macos_collector::client::init_key(args.force)? {
                    println!("study key created in the login keychain");
                } else {
                    println!("study key already exists in the login keychain");
                }
                Ok(())
            }
            ObserveCommand::Run(args) => loop {
                match eidos_macos_collector::client::load_session_key(&args.socket) {
                    Ok(()) => std::thread::sleep(std::time::Duration::from_secs(60)),
                    Err(error) => {
                        tracing::warn!(error = %error, "collector session handoff failed");
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    }
                }
            },
            ObserveCommand::Status(args) => {
                let response =
                    eidos_macos_collector::client::request(&args.socket, &Request::Status)?;
                match response {
                    Response::Status { status } => {
                        println!("{}", serde_json::to_string_pretty(&status)?);
                        Ok(())
                    }
                    Response::Error { message } => anyhow::bail!(message),
                    _ => anyhow::bail!("unexpected collector response"),
                }
            }
            ObserveCommand::Mark { label, socket } => {
                eidos_macos_collector::client::load_session_key(&socket.socket)?;
                match eidos_macos_collector::client::request(
                    &socket.socket,
                    &Request::Mark { label },
                )? {
                    Response::Accepted => Ok(()),
                    Response::Error { message } => anyhow::bail!(message),
                    _ => anyhow::bail!("unexpected collector response"),
                }
            }
            ObserveCommand::Export { output, socket } => {
                let response =
                    eidos_macos_collector::client::request(&socket.socket, &Request::Export)?;
                let Response::Exported { staged_file } = response else {
                    if let Response::Error { message } = response {
                        anyhow::bail!(message);
                    }
                    anyhow::bail!("unexpected collector response");
                };
                let mut source = std::fs::File::open(staged_file)?;
                let mut destination = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output)?;
                std::io::copy(&mut source, &mut destination)?;
                println!("{}", output.display());
                Ok(())
            }
            ObserveCommand::Inspect { .. } => unreachable!("handled by the portable path"),
        }
    }
}
