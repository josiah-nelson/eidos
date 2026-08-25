//! `eidos service` on macOS: lifecycle for a per-user launchd agent.
//!
//! The command surface matches the Windows service host — install, start,
//! stop, restart, status, uninstall, and a hidden `run` the supervisor
//! invokes — so operators and scripts learn one vocabulary. What differs is
//! what the words mean underneath:
//!
//! - `install` writes `~/Library/LaunchAgents/<label>.plist`. `start`
//!   bootstraps that job into the user's GUI domain and `stop` boots it out,
//!   leaving the file in place, which is the closest launchd equivalent of an
//!   SCM registration that exists while stopped.
//! - The agent is a **LaunchAgent running as the user**, not a root
//!   LaunchDaemon. It indexes the user's files, so it needs the user's
//!   privacy grants and the shares mounted in their session; a root daemon
//!   has neither. (The observatory collector is a daemon precisely because it
//!   measures the machine rather than one person's files.)
//!
//! Full Disk Access is only properly supported for executables inside an app
//! bundle, so `install` says so when it is asked to register a loose binary.

use crate::logging;
use crate::ServeArgs;
use anyhow::{bail, Context};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_LABEL: &str = "com.jnel.eidos.agent";

#[derive(Args, Debug, Clone)]
pub struct ServiceArgs {
    /// launchd label for the agent job.
    #[arg(long, global = true, default_value = DEFAULT_LABEL)]
    pub label: String,
    #[command(subcommand)]
    pub command: ServiceCommand,
}

impl ServiceArgs {
    pub fn is_run(&self) -> bool {
        matches!(self.command, ServiceCommand::Run(_))
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum ServiceCommand {
    /// Write the LaunchAgent (does not load it unless --start-now).
    Install(InstallArgs),
    /// Unload the agent and remove its LaunchAgent. Indexed data is kept.
    Uninstall,
    /// Load the agent into the user's session and wait until it answers.
    Start(WaitArgs),
    /// Unload the agent, leaving its LaunchAgent in place.
    Stop(WaitArgs),
    /// Stop then start.
    Restart(WaitArgs),
    /// Registration, load state, and (when running) API health.
    Status(StatusArgs),
    /// launchd entry point. Not for interactive use.
    #[command(hide = true)]
    Run(ServeArgs),
}

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[command(flatten)]
    pub serve: ServeArgs,
    /// Replace an existing LaunchAgent with the same label.
    #[arg(long)]
    pub replace: bool,
    /// Load the agent as soon as it is written.
    #[arg(long)]
    pub start_now: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WaitArgs {
    /// Seconds to wait for the state change before giving up.
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    /// Print JSON instead of text.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: ServiceArgs, log_filter: String, log_json: bool) -> anyhow::Result<()> {
    let label = args.label.clone();
    match args.command {
        ServiceCommand::Run(serve) => run_under_launchd(serve, log_filter, log_json),
        ServiceCommand::Install(install) => cmd_install(&label, install),
        ServiceCommand::Uninstall => cmd_uninstall(&label),
        ServiceCommand::Start(wait) => cmd_start(&label, wait),
        ServiceCommand::Stop(wait) => cmd_stop(&label, wait),
        ServiceCommand::Restart(wait) => {
            // One deadline for the whole restart: `--timeout` is what the
            // command may take, not what each half may take.
            let deadline = deadline(wait.timeout);
            stop_by(&label, deadline)?;
            start_by(&label, deadline)
        }
        ServiceCommand::Status(status) => cmd_status(&label, status),
    }
}

// ----- launchd plumbing ----------------------------------------------------

fn uid() -> u32 {
    // SAFETY: `getuid` has no failure mode and no arguments.
    unsafe { libc::getuid() }
}

fn domain() -> String {
    format!("gui/{}", uid())
}

fn service_target(label: &str) -> String {
    format!("{}/{label}", domain())
}

fn agents_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set; cannot locate LaunchAgents")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents"))
}

/// launchd labels are reverse-DNS names, and this one also becomes a file name
/// under `~/Library/LaunchAgents`. Anything that could climb out of that
/// directory would let `--replace` overwrite, or `uninstall` delete, a
/// registration belonging to something else.
fn validated_label(label: &str) -> anyhow::Result<&str> {
    let acceptable = !label.is_empty()
        && label.len() <= 255
        && label != "."
        && label != ".."
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !acceptable {
        bail!(
            "invalid label {label:?}: use a reverse-DNS name of letters, digits, dots, dashes, and underscores"
        );
    }
    Ok(label)
}

fn plist_path(label: &str) -> anyhow::Result<PathBuf> {
    Ok(agents_dir()?.join(format!("{}.plist", validated_label(label)?)))
}

struct LaunchctlOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

fn launchctl(args: &[&str]) -> anyhow::Result<LaunchctlOutput> {
    let output = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .with_context(|| format!("running launchctl {}", args.join(" ")))?;
    Ok(LaunchctlOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn launchctl_ok(args: &[&str]) -> anyhow::Result<()> {
    let out = launchctl(args)?;
    if out.status != 0 {
        let detail = if out.stderr.trim().is_empty() {
            out.stdout.trim().to_string()
        } else {
            out.stderr.trim().to_string()
        };
        bail!(
            "launchctl {} failed ({}): {detail}",
            args.join(" "),
            out.status
        );
    }
    Ok(())
}

/// What `launchctl print` says about the job right now.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
struct JobState {
    loaded: bool,
    running: bool,
    pid: Option<u32>,
    last_exit_status: Option<i32>,
}

fn job_state(label: &str) -> anyhow::Result<JobState> {
    let out = launchctl(&["print", &service_target(label)])?;
    if out.status != 0 {
        return Ok(JobState::default());
    }
    let mut state = JobState {
        loaded: true,
        ..Default::default()
    };
    for line in out.stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("pid = ") {
            state.pid = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("state = ") {
            state.running = value.trim() == "running";
        } else if let Some(value) = line.strip_prefix("last exit code = ") {
            state.last_exit_status = value.trim().parse().ok();
        }
    }
    state.running = state.running || state.pid.is_some();
    Ok(state)
}

/// Poll until `ready`, or until `deadline`. One deadline covers a whole
/// operation, so `--timeout` bounds the time a command can take rather than
/// the time each of its phases can take.
fn wait_until(
    deadline: std::time::Instant,
    mut ready: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    loop {
        if ready()? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn deadline(timeout_s: u64) -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(timeout_s)
}

// ----- install / uninstall -------------------------------------------------

fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute)
}

/// The executable launchd should start. Resolved once at install time so a
/// later `cargo build` in a working copy cannot silently change what the
/// agent runs.
fn program() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Whether `exe` lives inside an app bundle. Full Disk Access is only fully
/// supported for bundled executables, so a loose binary is worth a warning
/// even though everything else works.
fn is_bundled(exe: &Path) -> bool {
    exe.ancestors()
        .any(|a| a.extension().is_some_and(|e| e == "app"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist_document(label: &str, program: &Path, arguments: &[String], log_dir: &Path) -> String {
    let mut program_arguments = String::new();
    for argument in std::iter::once(program.display().to_string()).chain(arguments.iter().cloned())
    {
        program_arguments.push_str(&format!(
            "        <string>{}</string>\n",
            xml_escape(&argument)
        ));
    }
    let out = log_dir.join("launchd.out.log");
    let err = log_dir.join("launchd.err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program_arguments}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        program_arguments = program_arguments,
        out = xml_escape(&out.display().to_string()),
        err = xml_escape(&err.display().to_string()),
    )
}

fn cmd_install(label: &str, args: InstallArgs) -> anyhow::Result<()> {
    let path = plist_path(label)?;
    if path.exists() && !args.replace {
        bail!(
            "{} already exists; pass --replace to overwrite it",
            path.display()
        );
    }
    let mut serve = args.serve.normalized();
    serve.data_dir = absolute(&serve.data_dir)?;
    let log_dir = match &serve.log_dir {
        Some(dir) => absolute(dir)?,
        None => serve.data_dir.join("logs"),
    };
    serve.log_dir = Some(log_dir.clone());
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("creating the log directory {}", log_dir.display()))?;
    let program = program()?;
    let mut arguments = vec![
        "service".to_string(),
        "--label".to_string(),
        label.to_string(),
        "run".to_string(),
    ];
    for argument in serve.to_command_line() {
        arguments.push(argument.to_string_lossy().into_owned());
    }
    let document = plist_document(label, &program, &arguments, &log_dir);

    std::fs::create_dir_all(agents_dir()?)?;
    // Write then rename so a partially written plist is never loadable, and
    // do the write *before* unloading anything: a failure here then leaves the
    // running agent exactly as it was.
    let temporary = path.with_extension("plist.new");
    std::fs::write(&temporary, document)
        .with_context(|| format!("writing {}", temporary.display()))?;
    let was_loaded = job_state(label)?.loaded;
    if was_loaded {
        // launchd keeps running the definition it already read, so the old job
        // has to be gone - not merely asked to go - before the new file lands.
        unload(label, deadline(30))?;
    }
    if let Err(e) = std::fs::rename(&temporary, &path) {
        // The replacement never landed. Put the agent back rather than
        // leaving the machine with no indexer.
        if was_loaded {
            let _ = launchctl(&["bootstrap", &domain(), &path.display().to_string()]);
        }
        let _ = std::fs::remove_file(&temporary);
        return Err(anyhow::Error::new(e))
            .with_context(|| format!("installing {}", path.display()));
    }

    println!("installed {} -> {}", label, path.display());
    if !is_bundled(&program) {
        println!(
            "note: {} is not inside an app bundle. Everything works, but Full Disk Access is only \
             fully supported for bundled executables, so sources under protected folders may be \
             unreadable until the agent is installed from Eidos.app.",
            program.display()
        );
    }
    if args.start_now || was_loaded {
        // A replacement that was running before must be running after: an
        // install is a configuration change, not an outage.
        start_by(label, deadline(120))?;
    } else {
        println!("run `eidos service start` to load it");
    }
    Ok(())
}

fn cmd_uninstall(label: &str) -> anyhow::Result<()> {
    let path = plist_path(label)?;
    let state = job_state(label)?;
    if state.loaded {
        launchctl_ok(&["bootout", &service_target(label)])?;
    }
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed {}", path.display());
    } else {
        println!("{} was not installed", path.display());
    }
    println!("indexed data was kept");
    Ok(())
}

// ----- start / stop --------------------------------------------------------

fn cmd_start(label: &str, wait: WaitArgs) -> anyhow::Result<()> {
    start_by(label, deadline(wait.timeout))
}

fn start_by(label: &str, deadline: std::time::Instant) -> anyhow::Result<()> {
    let path = plist_path(label)?;
    if !path.exists() {
        bail!(
            "{} is not installed; run `eidos service install` first",
            path.display()
        );
    }
    load(label, &path)?;
    let started = wait_until(deadline, || Ok(job_state(label)?.running))?;
    if !started {
        bail!("the agent did not start in time; see `eidos service status`");
    }
    // launchd reports "running" as soon as it spawns the process, which is
    // before the API is listening. Callers mean "ready to answer", so wait
    // for that too when the installed plist says where to ask.
    if bind_address(&path).is_some() {
        let answering = wait_until(deadline, || Ok(probe_health(&path).is_some()))?;
        if !answering {
            bail!("the agent started but did not answer in time; see the log directory");
        }
    }
    println!("started {label}");
    Ok(())
}

/// Get the job running, whichever state launchd is in.
///
/// `bootout` is asynchronous, so a job can still look loaded a moment after it
/// was removed and look absent a moment after it was added. Each command is
/// therefore tried and, if launchd disagrees about which state it is in, the
/// other one is used rather than failing the operation.
fn load(label: &str, path: &Path) -> anyhow::Result<()> {
    let target = service_target(label);
    let plist = path.display().to_string();
    if job_state(label)?.loaded {
        if launchctl_ok(&["kickstart", &target]).is_ok() {
            return Ok(());
        }
        let _ = launchctl(&["bootout", &target])?;
        return launchctl_ok(&["bootstrap", &domain(), &plist]);
    }
    if launchctl_ok(&["bootstrap", &domain(), &plist]).is_ok() {
        return Ok(());
    }
    launchctl_ok(&["kickstart", &target])
}

/// Unload the job and wait for launchd to finish doing it.
fn unload(label: &str, deadline: std::time::Instant) -> anyhow::Result<bool> {
    let _ = launchctl(&["bootout", &service_target(label)])?;
    wait_until(deadline, || Ok(!job_state(label)?.loaded))
}

fn cmd_stop(label: &str, wait: WaitArgs) -> anyhow::Result<()> {
    stop_by(label, deadline(wait.timeout))
}

fn stop_by(label: &str, deadline: std::time::Instant) -> anyhow::Result<()> {
    let state = job_state(label)?;
    if !state.loaded {
        println!("{label} is not loaded");
        return Ok(());
    }
    // `bootout` unloads the job. `KeepAlive` would restart a merely killed
    // process, so this is what "stopped but still installed" means here.
    if !unload(label, deadline)? {
        bail!("the agent did not unload in time");
    }
    println!("stopped {label}");
    Ok(())
}

// ----- status --------------------------------------------------------------

#[derive(serde::Serialize)]
struct StatusReport {
    label: String,
    plist: String,
    installed: bool,
    #[serde(flatten)]
    job: JobState,
    health: Option<serde_json::Value>,
}

fn cmd_status(label: &str, args: StatusArgs) -> anyhow::Result<()> {
    let path = plist_path(label)?;
    let job = job_state(label)?;
    let health = job.running.then(|| probe_health(&path)).flatten();
    let report = StatusReport {
        label: label.to_string(),
        plist: path.display().to_string(),
        installed: path.exists(),
        job,
        health,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("label:     {}", report.label);
    println!("installed: {} ({})", report.installed, report.plist);
    println!(
        "loaded:    {}{}",
        report.job.loaded,
        match report.job.pid {
            Some(pid) => format!(" (pid {pid})"),
            None => String::new(),
        }
    );
    println!("running:   {}", report.job.running);
    if let Some(status) = report.job.last_exit_status {
        println!("last exit: {status}");
    }
    match &report.health {
        Some(health) => println!("health:    {health}"),
        None if report.job.running => println!("health:    not answering"),
        None => {}
    }
    Ok(())
}

/// The listen address recorded in an installed plist: the value of the
/// `<string>` right after the one holding `--bind`.
fn bind_address(plist: &Path) -> Option<String> {
    let document = std::fs::read_to_string(plist).ok()?;
    let mut lines = document.lines().map(str::trim);
    lines.find(|line| line.contains("<string>--bind</string>"))?;
    let value = lines.next()?;
    let value = value
        .strip_prefix("<string>")?
        .strip_suffix("</string>")?
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// A listen address turned into one a client can connect to. `0.0.0.0` and
/// `[::]` mean "every interface" to a listener and nothing at all to a
/// connector, so they become loopback.
fn routable(bind: &str) -> Option<String> {
    let address: std::net::SocketAddr = bind.parse().ok()?;
    if !address.ip().is_unspecified() {
        return Some(bind.to_string());
    }
    Some(match address.ip() {
        std::net::IpAddr::V4(_) => format!("127.0.0.1:{}", address.port()),
        std::net::IpAddr::V6(_) => format!("[::1]:{}", address.port()),
    })
}

/// Ask the running agent for `/api/health` on the address recorded in the
/// installed plist, so status reflects the service that was actually
/// installed rather than a default.
fn probe_health(plist: &Path) -> Option<serde_json::Value> {
    let bind = routable(&bind_address(plist)?)?;
    let url = format!("http://{bind}/api/health");
    // The probe runs inside a deadline-bounded poll, so it must not be able
    // to block on the operating system's own connect timeout.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .build()
        .into();
    agent
        .get(&url)
        .call()
        .ok()?
        .body_mut()
        .read_json::<serde_json::Value>()
        .ok()
}

// ----- the launchd entry point ---------------------------------------------

fn run_under_launchd(serve: ServeArgs, log_filter: String, log_json: bool) -> anyhow::Result<()> {
    let serve = serve.normalized();
    let log_dir = serve
        .log_dir
        .clone()
        .unwrap_or_else(|| serve.data_dir.join("logs"));
    // The rolling file log is the durable one and is opened before anything
    // else can fail. Nothing writes to stderr: launchd captures it to a file
    // where terminal colour codes would only be noise, and the plist's
    // capture is there for a panic that never reaches the logger.
    let _guard = logging::init(&log_filter, log_json, Some(&log_dir), false)?;
    tracing::info!(
        data_dir = %serve.data_dir.display(),
        bind = %serve.bind,
        "starting the eidos agent under launchd"
    );
    eidos_service::run_with(
        serve.service_config(),
        |address| tracing::info!(%address, "agent listening"),
        async {
            // launchd asks a job to stop with SIGTERM and escalates if it is
            // ignored, so a clean drain has to be wired to it explicitly.
            let mut terminate =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(signal) => signal,
                    Err(e) => {
                        tracing::error!(error = %e, "cannot listen for SIGTERM");
                        return;
                    }
                };
            tokio::select! {
                _ = terminate.recv() => tracing::info!("SIGTERM received; draining"),
                _ = tokio::signal::ctrl_c() => tracing::info!("interrupted; draining"),
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_starts_the_recorded_program_with_its_arguments() {
        let document = plist_document(
            "com.example.agent",
            Path::new("/Applications/Eidos.app/Contents/MacOS/eidos"),
            &[
                "service".into(),
                "run".into(),
                "--data-dir".into(),
                "/tmp/d".into(),
            ],
            Path::new("/tmp/d/logs"),
        );
        assert!(document.contains("<string>com.example.agent</string>"));
        assert!(
            document.contains("<string>/Applications/Eidos.app/Contents/MacOS/eidos</string>"),
            "{document}"
        );
        assert!(document.contains("<string>--data-dir</string>"));
        assert!(document.contains("<string>/tmp/d/logs/launchd.err.log</string>"));
        // A job that exits cleanly on request must not be restarted forever.
        assert!(document.contains("<key>SuccessfulExit</key>\n        <false/>"));
    }

    #[test]
    fn plist_values_are_escaped() {
        let document = plist_document(
            "com.example.agent",
            Path::new("/tmp/a & b/eidos"),
            &["--name".into(), "<odd>".into()],
            Path::new("/tmp/logs"),
        );
        assert!(document.contains("/tmp/a &amp; b/eidos"), "{document}");
        assert!(document.contains("&lt;odd&gt;"), "{document}");
        assert!(!document.contains("<odd>"), "{document}");
    }

    #[test]
    fn the_installed_listen_address_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("agent.plist");
        std::fs::write(
            &plist,
            plist_document(
                "com.example.agent",
                Path::new("/usr/local/bin/eidos"),
                &[
                    "service".into(),
                    "run".into(),
                    "--bind".into(),
                    "127.0.0.1:7788".into(),
                ],
                Path::new("/tmp/logs"),
            ),
        )
        .unwrap();
        assert_eq!(bind_address(&plist).as_deref(), Some("127.0.0.1:7788"));
    }

    #[test]
    fn an_address_free_plist_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("agent.plist");
        std::fs::write(
            &plist,
            plist_document(
                "com.example.agent",
                Path::new("/usr/local/bin/eidos"),
                &["service".into(), "run".into()],
                Path::new("/tmp/logs"),
            ),
        )
        .unwrap();
        assert_eq!(bind_address(&plist), None);
    }

    #[test]
    fn a_label_cannot_escape_the_registration_directory() {
        assert!(validated_label("com.jnel.eidos.agent").is_ok());
        assert!(validated_label("com.jnel.eidos.agent-test_2").is_ok());
        for bad in [
            "",
            ".",
            "..",
            "../../../etc/passwd",
            "com.jnel/../other",
            "com.jnel.eidos agent",
            "com.jnel\0agent",
        ] {
            assert!(
                validated_label(bad).is_err(),
                "{bad:?} must not be accepted as a label"
            );
        }
    }

    #[test]
    fn a_wildcard_listener_is_probed_on_loopback() {
        // "every interface" is not an address a client can dial.
        assert_eq!(routable("0.0.0.0:7700").as_deref(), Some("127.0.0.1:7700"));
        assert_eq!(routable("[::]:7700").as_deref(), Some("[::1]:7700"));
        assert_eq!(
            routable("127.0.0.1:7700").as_deref(),
            Some("127.0.0.1:7700")
        );
        assert_eq!(
            routable("192.0.2.10:7700").as_deref(),
            Some("192.0.2.10:7700")
        );
        assert_eq!(routable("not-an-address").as_deref(), None);
    }

    #[test]
    fn a_bundled_executable_is_recognised() {
        assert!(is_bundled(Path::new(
            "/Applications/Eidos.app/Contents/MacOS/eidos"
        )));
        assert!(!is_bundled(Path::new("/usr/local/bin/eidos")));
    }
}
