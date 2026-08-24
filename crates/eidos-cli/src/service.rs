//! `eidos service`: run eidos under the Windows Service Control Manager.
//!
//! `install` registers `eidos.exe service run <serve flags>` with the SCM,
//! storing every serve flag explicitly on the service command line so the
//! registration never depends on the environment or on a later version's
//! defaults. `run` is the SCM entry point: it reports start progress while
//! the catalog opens and recovers, logs only to files (there is no console),
//! and turns Stop/Shutdown into the same graceful shutdown Ctrl-C gives the
//! foreground `serve`.
//!
//! Nothing here deletes indexed data. `uninstall` removes the registration
//! and says where the data directory is.

use crate::{logging, ServeArgs};
use anyhow::{anyhow, bail, Context};
use clap::{Args, Subcommand, ValueEnum};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const DEFAULT_NAME: &str = "eidos";
const DESCRIPTION: &str =
    "eidos filesystem indexer: catalog, change feeds, content index, and local web UI.";

// Win32 error codes surfaced as friendly messages.
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
const ERROR_SERVICE_EXISTS: i32 = 1073;
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
const ERROR_SERVICE_NEVER_STARTED: u32 = 1077;

#[derive(Args, Debug, Clone)]
pub struct ServiceArgs {
    /// Service name registered with the SCM.
    #[arg(long, global = true, default_value = DEFAULT_NAME)]
    pub name: String,
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
    /// Register the service with the SCM (does not start it unless --start-now).
    Install(InstallArgs),
    /// Stop the service if running and remove its registration. Indexed data is kept.
    Uninstall(WaitArgs),
    /// Start the service and wait until it is running.
    Start(WaitArgs),
    /// Stop the service and wait until it has stopped.
    Stop(WaitArgs),
    /// Stop then start the service.
    Restart(WaitArgs),
    /// Registration, state, and (when running) API health.
    Status(StatusArgs),
    /// SCM entry point. Not for interactive use.
    #[command(hide = true)]
    Run(ServeArgs),
}

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[command(flatten)]
    pub serve: ServeArgs,
    /// Account the service runs as. LocalSystem sees local disks only;
    /// `user` is required for SMB/mapped-share sources.
    #[arg(long, value_enum, default_value_t = Account::LocalSystem)]
    pub account: Account,
    /// Windows account (DOMAIN\user) for --account user. Defaults to the
    /// current user.
    #[arg(long)]
    pub user: Option<String>,
    /// Password for --user. Prefer --password-stdin or the interactive prompt.
    #[arg(long, conflicts_with = "password_stdin")]
    pub password: Option<String>,
    /// Read the password for --user from the first line of stdin.
    #[arg(long)]
    pub password_stdin: bool,
    /// How the service starts at boot.
    #[arg(long, value_enum, default_value_t = StartMode::Delayed)]
    pub start: StartMode,
    /// Display name shown in Services.
    #[arg(long, default_value = "eidos")]
    pub display_name: String,
    /// Replace an existing registration with the same name.
    #[arg(long)]
    pub replace: bool,
    /// Start the service once it is installed.
    #[arg(long)]
    pub start_now: bool,
    /// Do not adjust the data/log directory ACLs for the service account.
    #[arg(long)]
    pub no_acl: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    /// Built-in LocalSystem: full local access, no network identity.
    LocalSystem,
    /// NT AUTHORITY\LocalService: minimal local rights, anonymous on the network.
    LocalService,
    /// NT AUTHORITY\NetworkService: minimal local rights, computer identity on the network.
    NetworkService,
    /// A named Windows account (--user, password).
    User,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// Automatic (delayed start): after boot-critical services.
    Delayed,
    /// Automatic at boot.
    Auto,
    /// Manual: started by an operator or the installer.
    Manual,
    /// Registered but disabled.
    Disabled,
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
    let name = args.name.clone();
    match args.command {
        ServiceCommand::Run(serve) => run_as_service(name, serve, log_filter, log_json),
        ServiceCommand::Install(install) => cmd_install(&name, install),
        ServiceCommand::Uninstall(wait) => cmd_uninstall(&name, wait),
        ServiceCommand::Start(wait) => cmd_start(&name, wait),
        ServiceCommand::Stop(wait) => cmd_stop(&name, wait),
        ServiceCommand::Restart(wait) => {
            cmd_stop(&name, wait.clone())?;
            cmd_start(&name, wait)
        }
        ServiceCommand::Status(status) => cmd_status(&name, status),
    }
}

// ----- install -------------------------------------------------------------

fn cmd_install(name: &str, args: InstallArgs) -> anyhow::Result<()> {
    let exe = current_exe()?;
    let mut serve = args.serve.clone();
    let serve = &mut serve;
    serve.data_dir = absolute(&serve.data_dir)?;
    let log_dir = match &serve.log_dir {
        Some(d) => absolute(d)?,
        None => serve.data_dir.join("logs"),
    };
    serve.log_dir = Some(log_dir.clone());
    if let Some(dir) = &serve.web_dir {
        serve.web_dir = Some(absolute(dir)?);
    }
    if !serve.bind.ip().is_loopback() {
        eprintln!(
            "warning: {} is not loopback; the API has no authentication in v0.5",
            serve.bind
        );
    }

    let (account_name, password) = resolve_account(&args)?;

    std::fs::create_dir_all(&serve.data_dir)
        .with_context(|| format!("creating data directory {}", serve.data_dir.display()))?;
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("creating log directory {}", log_dir.display()))?;

    if let Some(account) = &account_name {
        if args.account == Account::User {
            let (domain, user) = split_account(account);
            if let Some(pw) = &password {
                validate_credentials(&domain, &user, pw)?;
            }
            grant_logon_as_service(account)
                .with_context(|| format!("granting 'Log on as a service' to {account}"))?;
            println!("granted 'Log on as a service' to {account}");
        }
        if !args.no_acl {
            for dir in [&serve.data_dir, &log_dir] {
                grant_modify(dir, account)?;
            }
            println!(
                "granted Modify on {} to {account}",
                serve.data_dir.display()
            );
        }
    }

    let mut launch: Vec<OsString> =
        vec!["service".into(), "--name".into(), name.into(), "run".into()];
    launch.extend(serve.to_command_line());
    let start_type = match args.start {
        StartMode::Delayed | StartMode::Auto => ServiceStartType::AutoStart,
        StartMode::Manual => ServiceStartType::OnDemand,
        StartMode::Disabled => ServiceStartType::Disabled,
    };
    let info = ServiceInfo {
        name: OsString::from(name),
        display_name: OsString::from(&args.display_name),
        service_type: ServiceType::OWN_PROCESS,
        start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: launch,
        dependencies: vec![],
        account_name: account_name.clone().map(OsString::from),
        account_password: password.map(OsString::from),
    };

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(describe)?;
    let access = ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::QUERY_CONFIG
        | ServiceAccess::START;
    let (service, replaced) = match manager.create_service(&info, access) {
        Ok(s) => (s, false),
        Err(e) if os_error(&e) == Some(ERROR_SERVICE_EXISTS) => {
            if !args.replace {
                bail!(
                    "service '{name}' is already installed; pass --replace to update it or `eidos service uninstall` first"
                );
            }
            let s = manager.open_service(name, access).map_err(describe)?;
            let st = s.query_status().map_err(describe)?;
            if st.current_state != ServiceState::Stopped {
                bail!(
                    "service '{name}' is {}; stop it before --replace",
                    state_name(st.current_state)
                );
            }
            s.change_config(&info).map_err(describe)?;
            (s, true)
        }
        Err(e) => return Err(describe(e)),
    };
    service.set_description(DESCRIPTION).map_err(describe)?;
    service
        .set_delayed_auto_start(args.start == StartMode::Delayed)
        .map_err(describe)?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(120),
                },
            ]),
        })
        .map_err(describe)?;
    // A clean exit with a non-zero code (open failed: locked data dir,
    // unwritable path) should also restart, not just a crash.
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(describe)?;
    // Give a large catalog time to flush on shutdown.
    service
        .set_preshutdown_timeout(Duration::from_secs(180))
        .map_err(describe)?;

    println!(
        "{} service '{name}' ({})",
        if replaced { "updated" } else { "installed" },
        args.display_name
    );
    println!("  executable: {}", exe.display());
    println!("  data dir:   {}", serve.data_dir.display());
    println!("  log dir:    {}", log_dir.display());
    println!("  bind:       http://{}", serve.bind);
    println!(
        "  account:    {}",
        account_name.as_deref().unwrap_or("LocalSystem")
    );
    println!("  start:      {}", start_mode_name(args.start));
    if args.start_now {
        drop(service);
        cmd_start(name, WaitArgs { timeout: 120 })?;
    } else {
        println!("start it with: eidos service start");
    }
    Ok(())
}

/// Account name for the SCM (`None` = LocalSystem) and the password, if any.
fn resolve_account(args: &InstallArgs) -> anyhow::Result<(Option<String>, Option<String>)> {
    Ok(match args.account {
        Account::LocalSystem => (None, None),
        Account::LocalService => (Some(r"NT AUTHORITY\LocalService".into()), None),
        Account::NetworkService => (Some(r"NT AUTHORITY\NetworkService".into()), None),
        Account::User => {
            let user = match &args.user {
                Some(u) if !u.trim().is_empty() => u.trim().to_string(),
                _ => current_user()?,
            };
            let user = if user.contains('\\') || user.contains('@') {
                user
            } else {
                format!(r".\{user}")
            };
            let password = if let Some(p) = &args.password {
                p.clone()
            } else if args.password_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_string()
            } else {
                rpassword::prompt_password(format!("Password for {user}: "))
                    .context("reading password")?
            };
            (Some(user), Some(password))
        }
    })
}

fn current_user() -> anyhow::Result<String> {
    let user = std::env::var("USERNAME").context("USERNAME is not set")?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".into());
    Ok(format!(r"{domain}\{user}"))
}

/// `DOMAIN\user` / `.\user` / `user@domain` → (domain, user) for LogonUser.
fn split_account(account: &str) -> (String, String) {
    if let Some((d, u)) = account.split_once('\\') {
        (d.to_string(), u.to_string())
    } else if let Some((u, d)) = account.split_once('@') {
        (d.to_string(), u.to_string())
    } else {
        (".".to_string(), account.to_string())
    }
}

// ----- lifecycle commands -----------------------------------------------------

fn open(name: &str, access: ServiceAccess) -> anyhow::Result<windows_service::service::Service> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(describe)?;
    manager.open_service(name, access).map_err(|e| {
        if os_error(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
            anyhow!("service '{name}' is not installed (eidos service install ...)")
        } else {
            describe(e)
        }
    })
}

fn cmd_start(name: &str, wait: WaitArgs) -> anyhow::Result<()> {
    let service = open(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    let st = service.query_status().map_err(describe)?;
    if st.current_state == ServiceState::Running {
        println!("service '{name}' is already running (pid {})", pid_str(&st));
        return Ok(());
    }
    if st.current_state == ServiceState::Stopped {
        service.start::<&OsStr>(&[]).map_err(describe)?;
    }
    let st = wait_for(
        &service,
        ServiceState::Running,
        Duration::from_secs(wait.timeout),
        name,
    )?;
    println!("service '{name}' is running (pid {})", pid_str(&st));
    Ok(())
}

fn cmd_stop(name: &str, wait: WaitArgs) -> anyhow::Result<()> {
    let service = open(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    let st = service.query_status().map_err(describe)?;
    if st.current_state == ServiceState::Stopped {
        println!("service '{name}' is not running");
        return Ok(());
    }
    if st.current_state != ServiceState::StopPending {
        match service.stop() {
            Ok(_) => {}
            Err(e) if os_error(&e) == Some(ERROR_SERVICE_NOT_ACTIVE) => {}
            Err(e) => return Err(describe(e)),
        }
    }
    wait_for(
        &service,
        ServiceState::Stopped,
        Duration::from_secs(wait.timeout),
        name,
    )?;
    println!("service '{name}' stopped");
    Ok(())
}

fn cmd_uninstall(name: &str, wait: WaitArgs) -> anyhow::Result<()> {
    let service = open(
        name,
        ServiceAccess::STOP
            | ServiceAccess::QUERY_STATUS
            | ServiceAccess::QUERY_CONFIG
            | ServiceAccess::DELETE,
    )?;
    let registered = service
        .query_config()
        .ok()
        .and_then(|c| parse_registered(&c.executable_path.to_string_lossy()));
    let st = service.query_status().map_err(describe)?;
    if st.current_state != ServiceState::Stopped {
        if st.current_state != ServiceState::StopPending {
            let _ = service.stop();
        }
        wait_for(
            &service,
            ServiceState::Stopped,
            Duration::from_secs(wait.timeout),
            name,
        )?;
        println!("service '{name}' stopped");
    }
    service.delete().map_err(describe)?;
    drop(service);
    // Deletion completes once every handle is closed; confirm it is gone.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS))
        {
            Err(e) if os_error(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => break,
            Err(e) if os_error(&e) == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {}
            Ok(_) => {}
            Err(e) => return Err(describe(e)),
        }
        if Instant::now() > deadline {
            println!("service '{name}' is marked for deletion; it disappears when the last handle (e.g. an open Services window) closes");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("uninstalled service '{name}'");
    if let Some(r) = registered {
        if let Some(d) = r.data_dir {
            println!("indexed data kept in {d}");
        }
    }
    Ok(())
}

fn wait_for(
    service: &windows_service::service::Service,
    target: ServiceState,
    timeout: Duration,
    name: &str,
) -> anyhow::Result<ServiceStatus> {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut last_checkpoint = u32::MAX;
    loop {
        let st = service.query_status().map_err(describe)?;
        if st.current_state == target {
            return Ok(st);
        }
        // Stopped while we waited for Running: the service failed to start.
        if target == ServiceState::Running && st.current_state == ServiceState::Stopped {
            bail!(
                "service '{name}' stopped during start ({}); see its log directory (eidos service status)",
                exit_str(&st)
            );
        }
        // Progress only for slow transitions (catalog recovery, index
        // rebuild); a sub-second start should print one line, not three.
        if st.checkpoint != last_checkpoint && started.elapsed() > Duration::from_secs(1) {
            last_checkpoint = st.checkpoint;
            eprintln!(
                "  {} ({}s, checkpoint {})",
                state_name(st.current_state),
                started.elapsed().as_secs(),
                st.checkpoint
            );
        }
        if Instant::now() > deadline {
            bail!(
                "timed out after {}s waiting for '{name}' to be {} (currently {})",
                timeout.as_secs(),
                state_name(target),
                state_name(st.current_state)
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ----- status ---------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
struct Registered {
    data_dir: Option<String>,
    log_dir: Option<String>,
    bind: Option<String>,
}

fn cmd_status(name: &str, args: StatusArgs) -> anyhow::Result<()> {
    let service = open(
        name,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    )?;
    let st = service.query_status().map_err(describe)?;
    let cfg = service.query_config().map_err(describe)?;
    let command_line = cfg.executable_path.to_string_lossy().into_owned();
    let registered = parse_registered(&command_line).unwrap_or_default();
    let delayed = matches!(cfg.start_type, ServiceStartType::AutoStart);
    let start = match cfg.start_type {
        ServiceStartType::AutoStart => "automatic",
        ServiceStartType::OnDemand => "manual",
        ServiceStartType::Disabled => "disabled",
        _ => "other",
    };
    let account = cfg
        .account_name
        .as_ref()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "LocalSystem".into());
    let health = if st.current_state == ServiceState::Running {
        registered.bind.as_deref().map(probe_health)
    } else {
        None
    };

    if args.json {
        let v = serde_json::json!({
            "name": name,
            "display_name": cfg.display_name.to_string_lossy(),
            "state": state_name(st.current_state),
            "pid": st.process_id,
            "exit": exit_str(&st),
            "start_type": start,
            "delayed": delayed,
            "account": account,
            "command_line": command_line,
            "data_dir": registered.data_dir,
            "log_dir": registered.log_dir,
            "bind": registered.bind,
            "api": health.as_ref().map(|h| match h { Ok(s) => s.clone(), Err(e) => format!("unreachable: {e}") }),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!(
        "service:     {name} ({})",
        cfg.display_name.to_string_lossy()
    );
    match st.current_state {
        ServiceState::Running => println!("state:       running (pid {})", pid_str(&st)),
        ServiceState::Stopped => println!("state:       stopped ({})", exit_str(&st)),
        s => println!(
            "state:       {} (checkpoint {})",
            state_name(s),
            st.checkpoint
        ),
    }
    println!("start:       {start}");
    println!("account:     {account}");
    println!("command:     {command_line}");
    if let Some(d) = &registered.data_dir {
        println!("data dir:    {d}");
    }
    if let Some(d) = &registered.log_dir {
        println!("log dir:     {d}");
    }
    if let Some(b) = &registered.bind {
        println!("url:         http://{b}/");
    }
    match health {
        Some(Ok(s)) => println!("api:         {s}"),
        Some(Err(e)) => println!("api:         unreachable ({e})"),
        None => {}
    }
    Ok(())
}

fn probe_health(bind: &str) -> Result<String, String> {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .new_agent();
    let url = format!("http://{}/api/health", bind.replace("0.0.0.0", "127.0.0.1"));
    match agent.get(&url).call() {
        Ok(resp) => {
            let code = resp.status().as_u16();
            if code == 200 {
                Ok(format!("healthy (GET /api/health {code})"))
            } else {
                Err(format!("GET /api/health returned {code}"))
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Recover the paths and bind address from a registered command line.
/// The registration is written by `install`, so the shape is known; this
/// still tolerates hand-edited registrations with quoted paths.
fn parse_registered(command_line: &str) -> Option<Registered> {
    let tokens = split_command_line(command_line);
    let mut r = Registered::default();
    let mut it = tokens.iter().peekable();
    let mut seen_run = false;
    while let Some(t) = it.next() {
        if !seen_run {
            seen_run = t == "run";
            continue;
        }
        match t.as_str() {
            "--data-dir" => r.data_dir = it.next().cloned(),
            "--log-dir" => r.log_dir = it.next().cloned(),
            "--bind" => r.bind = it.next().cloned(),
            _ => {}
        }
    }
    seen_run.then_some(r)
}

/// Minimal Windows command-line tokenizer: whitespace-separated, double
/// quotes group, `""` inside quotes is a literal quote. Enough for paths.
fn split_command_line(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                    has_token = true;
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

// ----- the service itself ----------------------------------------------------

struct RunContext {
    name: String,
    serve: ServeArgs,
    log_filter: String,
    log_json: bool,
    log_dir: PathBuf,
}

static RUN: OnceLock<RunContext> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

fn run_as_service(
    name: String,
    serve: ServeArgs,
    log_filter: String,
    log_json: bool,
) -> anyhow::Result<()> {
    let serve = serve.normalized();
    let log_dir = serve
        .log_dir
        .clone()
        .unwrap_or_else(|| serve.data_dir.join("logs"));
    let _ = RUN.set(RunContext {
        name: name.clone(),
        serve,
        log_filter,
        log_json,
        log_dir,
    });
    match service_dispatcher::start(&name, ffi_service_main) {
        Ok(()) => Ok(()),
        Err(e) if os_error(&e) == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) => bail!(
            "`eidos service run` is the Windows service entry point and must be started by the service control manager; use `eidos serve` to run in the foreground or `eidos service start`"
        ),
        Err(e) => Err(describe(e)),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let ctx = RUN.get().expect("run context set before dispatch");
    let _log_guard = logging::init(&ctx.log_filter, ctx.log_json, Some(&ctx.log_dir), false);
    tracing::info!(name = %ctx.name, version = env!("CARGO_PKG_VERSION"), "service starting");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = Arc::new(Mutex::new(Some(stop_tx)));
    let status_slot: Arc<Mutex<Option<service_control_handler::ServiceStatusHandle>>> =
        Arc::new(Mutex::new(None));

    let handler = {
        let stop_tx = stop_tx.clone();
        let status_slot = status_slot.clone();
        move |control: ServiceControl| match control {
            ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                tracing::info!(?control, "stop requested by the service control manager");
                if let Some(h) = *status_slot.lock().unwrap() {
                    let _ = h.set_service_status(status(ServiceState::StopPending, 0, 180));
                }
                if let Some(tx) = stop_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let handle = match service_control_handler::register(&ctx.name, handler) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "cannot register the service control handler");
            return;
        }
    };
    *status_slot.lock().unwrap() = Some(handle);
    let _ = handle.set_service_status(status(ServiceState::StartPending, 1, 60));

    // Opening a large catalog (startup recovery, index rebuild) can take a
    // while; keep the SCM informed so it does not time the start out.
    let ready = Arc::new(AtomicBool::new(false));
    {
        let ready = ready.clone();
        std::thread::Builder::new()
            .name("scm-start-progress".into())
            .spawn(move || {
                let mut checkpoint = 2u32;
                while !ready.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(5));
                    if ready.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = handle.set_service_status(status(
                        ServiceState::StartPending,
                        checkpoint,
                        60,
                    ));
                    checkpoint += 1;
                }
            })
            .expect("spawn start progress");
    }

    let config = ctx.serve.service_config();
    if !config.bind.ip().is_loopback() {
        tracing::warn!(bind = %config.bind, "binding beyond loopback: the API has no authentication in v0.5");
    }
    let on_ready = {
        let ready = ready.clone();
        move |addr: std::net::SocketAddr| {
            ready.store(true, Ordering::Relaxed);
            let _ = handle.set_service_status(status(ServiceState::Running, 0, 0));
            tracing::info!(url = %format!("http://{addr}/"), "service running");
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eidos_service::run_with(config, on_ready, async move {
            let _ = stop_rx.await;
        })
    }));
    ready.store(true, Ordering::Relaxed);
    let exit = match result {
        Ok(Ok(())) => {
            tracing::info!("service stopped");
            ServiceExitCode::Win32(0)
        }
        Ok(Err(e)) => {
            tracing::error!(error = %format!("{e:#}"), "service failed");
            ServiceExitCode::ServiceSpecific(1)
        }
        Err(_) => {
            tracing::error!("service panicked");
            ServiceExitCode::ServiceSpecific(2)
        }
    };
    let mut st = status(ServiceState::Stopped, 0, 0);
    st.exit_code = exit;
    let _ = handle.set_service_status(st);
}

fn status(state: ServiceState, checkpoint: u32, wait_hint_s: u64) -> ServiceStatus {
    let controls = if state == ServiceState::Running {
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::PRESHUTDOWN
    } else {
        ServiceControlAccept::empty()
    };
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: controls,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint,
        wait_hint: Duration::from_secs(wait_hint_s),
        process_id: None,
    }
}

// ----- Win32 helpers ----------------------------------------------------------

fn current_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating eidos.exe")?;
    // `canonicalize` yields a `\\?\` path, which the SCM stores verbatim and
    // Services shows to the user; keep the plain form.
    Ok(strip_verbatim(exe))
}

fn absolute(p: &Path) -> anyhow::Result<PathBuf> {
    Ok(strip_verbatim(std::path::absolute(p)?))
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => match s.strip_prefix(r"\\?\") {
            Some(rest) => PathBuf::from(rest),
            None => p,
        },
    }
}

/// Give `account` Modify (read/write/delete, inherited by children) on
/// `dir` so a non-LocalSystem service can write its catalog and logs.
fn grant_modify(dir: &Path, account: &str) -> anyhow::Result<()> {
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/grant")
        .arg(format!("{account}:(OI)(CI)M"))
        .output()
        .context("running icacls")?;
    if !out.status.success() {
        bail!(
            "icacls failed on {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Check a username/password pair without needing any logon right yet.
fn validate_credentials(domain: &str, user: &str, password: &str) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        LogonUserW, LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT,
    };
    let u = wide(user);
    let d = wide(domain);
    let p = wide(password);
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: all strings are NUL-terminated wide buffers that outlive the call.
    let ok = unsafe {
        LogonUserW(
            u.as_ptr(),
            d.as_ptr(),
            p.as_ptr(),
            LOGON32_LOGON_NETWORK,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        bail!("cannot log on as {domain}\\{user}: {e}");
    }
    // SAFETY: token is a valid handle returned by LogonUserW.
    unsafe { CloseHandle(token) };
    Ok(())
}

/// Add `SeServiceLogonRight` for `account` through the Local Security
/// Authority. Idempotent.
fn grant_logon_as_service(account: &str) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Security::Authentication::Identity::{
        LsaAddAccountRights, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy, LSA_HANDLE,
        LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
    };
    use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};

    let name = wide(account);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut sid_use: SID_NAME_USE = 0;
    // SAFETY: first call sizes the buffers; a NULL buffer is documented.
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut sid_use,
        );
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        bail!("account '{account}' not found: {err}");
    }
    let mut sid = vec![0u8; sid_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: buffers are sized by the previous call.
    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut sid_use,
        )
    };
    if ok == 0 {
        bail!(
            "account '{account}' not found: {}",
            std::io::Error::last_os_error()
        );
    }

    // SAFETY: zeroed attributes with Length set is the documented form.
    let mut attrs: LSA_OBJECT_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attrs.Length = std::mem::size_of::<LSA_OBJECT_ATTRIBUTES>() as u32;
    let mut policy: LSA_HANDLE = 0;
    // SAFETY: NULL system name = local machine; attrs outlives the call.
    let st = unsafe {
        LsaOpenPolicy(
            std::ptr::null(),
            &attrs,
            (POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT) as u32,
            &mut policy,
        )
    };
    if st != 0 {
        // SAFETY: converting an NTSTATUS is pure.
        let code = unsafe { LsaNtStatusToWinError(st) };
        bail!(
            "LsaOpenPolicy: {}",
            std::io::Error::from_raw_os_error(code as i32)
        );
    }
    let mut right = wide("SeServiceLogonRight");
    let chars = right.len() - 1;
    let lsa_right = LSA_UNICODE_STRING {
        Length: (chars * 2) as u16,
        MaximumLength: ((chars + 1) * 2) as u16,
        Buffer: right.as_mut_ptr(),
    };
    // SAFETY: policy is open; sid and lsa_right outlive the call.
    let st = unsafe { LsaAddAccountRights(policy, sid.as_mut_ptr().cast(), &lsa_right, 1) };
    // SAFETY: closing the handle we opened.
    unsafe { LsaClose(policy) };
    if st != 0 {
        // SAFETY: pure conversion.
        let code = unsafe { LsaNtStatusToWinError(st) };
        bail!(
            "LsaAddAccountRights: {}",
            std::io::Error::from_raw_os_error(code as i32)
        );
    }
    Ok(())
}

fn os_error(e: &windows_service::Error) -> Option<i32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

fn describe(e: windows_service::Error) -> anyhow::Error {
    match os_error(&e) {
        Some(ERROR_ACCESS_DENIED) => {
            anyhow!("access denied: run this from an elevated (Administrator) prompt")
        }
        Some(ERROR_SERVICE_DOES_NOT_EXIST) => anyhow!("the service is not installed"),
        Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {
            anyhow!("the service is marked for deletion; close Services windows and retry")
        }
        _ => anyhow!("{e}"),
    }
}

fn state_name(s: ServiceState) -> &'static str {
    match s {
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "starting",
        ServiceState::StopPending => "stopping",
        ServiceState::Running => "running",
        ServiceState::ContinuePending => "resuming",
        ServiceState::PausePending => "pausing",
        ServiceState::Paused => "paused",
    }
}

fn start_mode_name(m: StartMode) -> &'static str {
    match m {
        StartMode::Delayed => "automatic (delayed)",
        StartMode::Auto => "automatic",
        StartMode::Manual => "manual",
        StartMode::Disabled => "disabled",
    }
}

fn pid_str(st: &ServiceStatus) -> String {
    st.process_id
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".into())
}

fn exit_str(st: &ServiceStatus) -> String {
    match st.exit_code {
        ServiceExitCode::Win32(0) => "exit 0".into(),
        ServiceExitCode::Win32(ERROR_SERVICE_NEVER_STARTED) => "not started since boot".into(),
        ServiceExitCode::Win32(c) => format!(
            "win32 error {c}: {}",
            std::io::Error::from_raw_os_error(c as i32)
        ),
        ServiceExitCode::ServiceSpecific(1) => {
            "exit 1: startup failed, see the log directory".into()
        }
        ServiceExitCode::ServiceSpecific(2) => "exit 2: panicked, see the log directory".into(),
        ServiceExitCode::ServiceSpecific(c) => format!("service-specific exit {c}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_command_lines() {
        assert_eq!(
            split_command_line(
                r#""C:\Program Files\eidos\eidos.exe" service --name eidos run --data-dir "C:\ProgramData\eidos data" --bind 127.0.0.1:7700"#
            ),
            vec![
                r"C:\Program Files\eidos\eidos.exe",
                "service",
                "--name",
                "eidos",
                "run",
                "--data-dir",
                r"C:\ProgramData\eidos data",
                "--bind",
                "127.0.0.1:7700",
            ]
        );
        assert_eq!(split_command_line(r#"a ""b"" c"#), vec!["a", "b", "c"]);
        assert_eq!(split_command_line(r#""x ""y"" z""#), vec![r#"x "y" z"#]);
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn parses_registered_paths_after_run() {
        let r = parse_registered(
            r#""C:\eidos\eidos.exe" service --name eidos run --data-dir "C:\ProgramData\eidos" --bind 127.0.0.1:7711 --log-dir C:\logs --no-content"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Registered {
                data_dir: Some(r"C:\ProgramData\eidos".into()),
                log_dir: Some(r"C:\logs".into()),
                bind: Some("127.0.0.1:7711".into()),
            }
        );
        assert_eq!(parse_registered(r#""C:\other.exe" --data-dir X"#), None);
    }

    #[test]
    fn strips_verbatim_prefixes() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\x\y")),
            PathBuf::from(r"C:\x\y")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\d")),
            PathBuf::from(r"\\server\share\d")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"D:\plain")),
            PathBuf::from(r"D:\plain")
        );
    }

    #[test]
    fn splits_account_forms() {
        assert_eq!(
            split_account(r"CORP\alice"),
            ("CORP".into(), "alice".into())
        );
        assert_eq!(
            split_account("alice@corp.example"),
            ("corp.example".into(), "alice".into())
        );
        assert_eq!(split_account("alice"), (".".into(), "alice".into()));
    }
}
