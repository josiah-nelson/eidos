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
    /// Windows: write the collector configuration (lanes, upload, excluded
    /// volumes) to the data directory, so an unattended install can set a
    /// host up before the service first starts.
    #[cfg(windows)]
    Configure(ConfigureArgs),
    /// Windows: register the collector as a LocalSystem service.
    #[cfg(windows)]
    Install(InstallArgs),
    /// Windows: stop and remove the collector service (data is kept).
    #[cfg(windows)]
    Uninstall,
    /// Windows: delete a collector data directory - spool, cursors, staged
    /// exports, logs, configuration, and the study key. The service must be
    /// removed first, and prior object tokens become unlinkable.
    #[cfg(windows)]
    Purge(PurgeArgs),
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
pub struct PurgeArgs {
    #[command(flatten)]
    data: DataDirArgs,
    /// Delete the directory even though it holds none of the collector's own
    /// files.
    #[arg(long)]
    force: bool,
}

#[cfg(windows)]
#[derive(Debug, Args)]
pub struct ConfigureArgs {
    #[command(flatten)]
    data: DataDirArgs,
    /// Exactly the lanes to enable: a comma-separated list of `usn`, `etw`,
    /// `content`, `enumeration`, or `all` / `none`. The individual switches
    /// below are applied afterwards and win.
    #[arg(long, value_name = "LIST")]
    lanes: Option<String>,
    /// L1: USN journal change traces on every journaled local volume.
    #[arg(long, value_name = "on|off", value_parser = parse_switch)]
    usn: Option<bool>,
    /// L2: ETW file-access lane, traced in randomized windows.
    #[arg(long, value_name = "on|off", value_parser = parse_switch)]
    etw: Option<bool>,
    /// L2: content economics probe on files closed after a write.
    #[arg(long, value_name = "on|off", value_parser = parse_switch)]
    content: Option<bool>,
    /// L2: periodic read-only enumeration of each fixed volume.
    #[arg(long, value_name = "on|off", value_parser = parse_switch)]
    enumeration: Option<bool>,
    /// Directory the daily bundle is copied into, typically a UNC share that
    /// grants write access to the machine account. A non-empty value turns
    /// the upload on; an empty one turns it off and clears the destination.
    #[arg(long, value_name = "PATH")]
    upload_destination: Option<String>,
    /// Local hour, 0-23, at or after which the day's upload runs.
    #[arg(long, value_name = "HOUR")]
    upload_hour: Option<u32>,
    /// Delivery attempts per day before the bundle waits for tomorrow.
    #[arg(long, value_name = "COUNT")]
    upload_attempts: Option<u32>,
    /// Turn the daily upload on or off without changing the destination.
    #[arg(long, value_name = "on|off", value_parser = parse_switch)]
    upload: Option<bool>,
    /// Drive letter or volume GUID path to leave alone entirely; repeat to
    /// name several. Giving any replaces the current list, and the single
    /// value `none` clears it - an empty list cannot say that, because
    /// giving nothing has to keep meaning "leave this host alone".
    #[arg(long = "exclude-volume", value_name = "VOLUME")]
    exclude_volumes: Vec<String>,
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
    use anyhow::Context;
    use eidos_windows_collector::client::{expect_accepted, request};
    use eidos_windows_collector::protocol::{Request, Response};
    use eidos_windows_collector::{keystore, service};

    pub fn run(command: ObserveCommand, log_filter: &str) -> anyhow::Result<()> {
        match command {
            ObserveCommand::Init(args) => init(args),
            ObserveCommand::Configure(args) => configure(args),
            ObserveCommand::Install(args) => service::install(&args.data.data_dir, args.start_now),
            ObserveCommand::Uninstall => service::uninstall(),
            ObserveCommand::Purge(args) => purge(args),
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
                let data_dir = service::normalize_data_dir(&args.data.data_dir)?;
                if args.service {
                    let _guard = eidos_windows_collector::log::init(&data_dir, log_filter, false)?;
                    service::run_service(data_dir)
                } else {
                    foreground(data_dir)
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
        // An unattended install passes the cohort key on every run, including
        // repairs and upgrades. Importing the key a host already has is a
        // no-op; importing a different one silently splits that host's tokens
        // in two, so it has to be asked for.
        if let Some(imported) = imported {
            if let Some(existing) = existing_key_matches(&args.data.data_dir, imported)? {
                if !existing && !args.force {
                    anyhow::bail!(
                        "a different study key already exists in {}; --force replaces it, and every token this host has already written becomes unlinkable from the ones it writes next",
                        args.data.data_dir.display()
                    );
                }
            }
        }
        // Restrict the ACL first: the key blob must never sit in a directory
        // that inherits read access for every user on the machine.
        let data_dir = &service::prepare_data_dir(&args.data.data_dir)?;
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

    /// `Some(true)` when the stored key is the one being imported, `Some(false)`
    /// when it is a different key, `None` when there is no key yet. Compared
    /// through a token, because a study key deliberately exposes no bytes.
    fn existing_key_matches(
        data_dir: &std::path::Path,
        imported: [u8; 32],
    ) -> anyhow::Result<Option<bool>> {
        const PROBE: &str = "key-identity";
        let Some(existing) = keystore::load(data_dir)? else {
            return Ok(None);
        };
        let mine = eidos_observe::StudyKey::from_bytes(imported);
        Ok(Some(
            existing.token(PROBE, b"eidos-observe/1").encoded()
                == mine.token(PROBE, b"eidos-observe/1").encoded(),
        ))
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

    /// Delete a collector data directory.
    ///
    /// The uninstaller calls this once the service is gone, because a
    /// Windows Installer file list is computed before the service stops:
    /// everything the collector writes in between - the spool's write-ahead
    /// log, a log line, a moved cursor - would survive it and keep the
    /// directory alive.
    fn purge(args: PurgeArgs) -> anyhow::Result<()> {
        let data_dir = service::normalize_data_dir(&args.data.data_dir)?;
        if !data_dir.exists() {
            println!("{} does not exist", data_dir.display());
            return Ok(());
        }
        if let Some(state) = service::registration()? {
            anyhow::bail!(
                "service {} is still installed ({state}); run `eidos observe uninstall` first",
                eidos_windows_collector::SERVICE_NAME
            );
        }
        // A registration check alone is not enough: `observe run` has no SCM
        // registration. The pipe is machine-wide, though, so only reject the
        // purge when the answering collector identifies this exact directory;
        // an unrelated foreground collector must not block cleanup here.
        if let Ok(Response::Status { status }) = request(&Request::Status) {
            if same_data_dir(&data_dir, &status.data_dir) {
                anyhow::bail!(
                    "a collector is running and using this data directory; stop it first"
                );
            }
        }
        if !holds_collector_data(&data_dir) && !args.force {
            anyhow::bail!(
                "{} holds no study key, configuration or spool; pass --force to delete it anyway",
                data_dir.display()
            );
        }
        std::fs::remove_dir_all(&data_dir)
            .with_context(|| format!("removing {}", data_dir.display()))?;
        println!("removed {}", data_dir.display());
        Ok(())
    }

    /// A guard against deleting a directory that was never the collector's:
    /// the data directory is an installer property, and a mistyped one must
    /// not take a directory of someone else's files with it.
    fn holds_collector_data(dir: &std::path::Path) -> bool {
        // Names only this collector writes. `config.json` is deliberately not
        // among them: it is every application's file name, and a mistyped
        // data directory must not be deletable because it happens to hold one.
        [eidos_windows_collector::keystore::KEY_FILE, "spool.db"]
            .iter()
            .any(|name| dir.join(name).exists())
    }

    fn same_data_dir(left: &std::path::Path, right: &std::path::Path) -> bool {
        match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => left
                .to_string_lossy()
                .eq_ignore_ascii_case(&right.to_string_lossy()),
        }
    }

    /// Write the configuration file the collector reads at start-up. Lanes
    /// can also be switched at run time over the pipe (`observe lanes`);
    /// everything else here takes effect the next time the service starts.
    fn configure(args: ConfigureArgs) -> anyhow::Result<()> {
        use eidos_windows_collector::config::CollectorConfig;

        let data_dir = service::normalize_data_dir(&args.data.data_dir)?;
        if !data_dir.exists() {
            anyhow::bail!(
                "no collector data directory at {}; run `eidos observe init` first",
                data_dir.display()
            );
        }
        let config = CollectorConfig::edit_locked(&data_dir, move |config| {
            if let Some(list) = &args.lanes {
                let (usn, etw, content, enumeration) = parse_lanes(list)?;
                config.lanes.usn = usn;
                config.lanes.etw.enabled = etw;
                config.lanes.content.enabled = content;
                config.lanes.enumeration.enabled = enumeration;
            }
            if let Some(on) = args.usn {
                config.lanes.usn = on;
            }
            if let Some(on) = args.etw {
                config.lanes.etw.enabled = on;
            }
            if let Some(on) = args.content {
                config.lanes.content.enabled = on;
            }
            if let Some(on) = args.enumeration {
                config.lanes.enumeration.enabled = on;
            }

            if let Some(destination) = args.upload_destination {
                let destination = destination.trim().to_string();
                // A path ending in a backslash escapes the closing quote of a
                // Windows command line, and everything after it arrives inside
                // this one argument. Refuse it rather than store a destination
                // that will never resolve: an installer that passed
                // `\\fileserver\share\` should fail, not configure a host
                // whose uploads quietly go nowhere.
                if destination.contains('"') {
                    anyhow::bail!(
                        "the upload destination contains a quote, which usually means it was given with a trailing backslash: {destination:?}"
                    );
                }
                config.upload.enabled = !destination.is_empty();
                config.upload.destination = destination;
            }
            if let Some(hour) = args.upload_hour {
                if hour > 23 {
                    anyhow::bail!("--upload-hour must be an hour of the day, 0-23");
                }
                config.upload.hour = hour;
            }
            if let Some(attempts) = args.upload_attempts {
                if attempts == 0 {
                    anyhow::bail!("--upload-attempts must be at least 1");
                }
                config.upload.attempts = attempts;
            }
            if let Some(on) = args.upload {
                if on && config.upload.destination.is_empty() {
                    anyhow::bail!("--upload on needs a destination; pass --upload-destination");
                }
                config.upload.enabled = on;
            }
            if !args.exclude_volumes.is_empty() {
                // `none` is the way to say "exclude nothing", for the same
                // reason `EIDOS_UPLOAD=none` exists: absent has to keep meaning
                // "do not touch", so emptiness needs a spelling of its own.
                let clearing = args.exclude_volumes.len() == 1
                    && args.exclude_volumes[0].trim().eq_ignore_ascii_case("none");
                config.exclude_volumes = if clearing {
                    Vec::new()
                } else {
                    args.exclude_volumes
                };
            }
            Ok(())
        })?;

        let shown = |on: bool| if on { "on" } else { "off" };
        println!("{}", CollectorConfig::path(&data_dir).display());
        println!(
            "lanes: usn {}, etw {}, content {}, enumeration {}",
            shown(config.lanes.usn),
            shown(config.lanes.etw.enabled),
            shown(config.lanes.content.enabled),
            shown(config.lanes.enumeration.enabled)
        );
        if config.upload.enabled {
            println!(
                "upload: {} at {:02}:00 local, {} attempts",
                config.upload.destination, config.upload.hour, config.upload.attempts
            );
        } else {
            println!("upload: off");
        }
        if !config.exclude_volumes.is_empty() {
            println!("excluded volumes: {}", config.exclude_volumes.join(", "));
        }
        if let Some(state) = service::registration()? {
            if state != "Stopped" {
                println!(
                    "service {} is {state}; the new configuration is read at its next start",
                    eidos_windows_collector::SERVICE_NAME
                );
            }
        }
        Ok(())
    }

    /// A `usn,etw` style lane list: the set named is exactly the set enabled.
    fn parse_lanes(list: &str) -> anyhow::Result<(bool, bool, bool, bool)> {
        let (mut usn, mut etw, mut content, mut enumeration) = (false, false, false, false);
        for name in list.split(',') {
            match name.trim().to_ascii_lowercase().as_str() {
                "" | "none" => {}
                "all" => (usn, etw, content, enumeration) = (true, true, true, true),
                "usn" => usn = true,
                "etw" => etw = true,
                "content" => content = true,
                "enumeration" => enumeration = true,
                other => anyhow::bail!(
                    "unknown lane {other:?}; expected usn, etw, content, enumeration, all or none"
                ),
            }
        }
        Ok((usn, etw, content, enumeration))
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn data_directory_identity_distinguishes_collectors() {
            let temp = tempfile::tempdir().unwrap();
            let active = temp.path().join("active");
            let inactive = temp.path().join("inactive");
            std::fs::create_dir_all(&active).unwrap();
            std::fs::create_dir_all(&inactive).unwrap();

            assert!(same_data_dir(&active, &active.join(".")));
            assert!(!same_data_dir(&active, &inactive));
        }
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
