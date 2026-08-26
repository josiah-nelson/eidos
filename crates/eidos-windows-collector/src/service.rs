//! Service Control Manager integration: registration as a LocalSystem
//! service with delayed automatic start and restart-on-failure, lifecycle
//! commands, and the dispatcher entry point that hands SCM controls
//! (stop, shutdown, power events) to the daemon as `ControlEvent`s.

use crate::daemon::{self, ControlEvent, Options};
use crate::SERVICE_NAME;
use anyhow::{bail, Context};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_service::service::{
    PowerEventParam, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
    ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

pub fn install(data_dir: &Path, start_now: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating eidos.exe")?;
    let data_dir = absolute(data_dir)?;
    std::fs::create_dir_all(&data_dir)?;
    restrict_acl(&data_dir)?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(describe)?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("eidos observatory collector"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: strip_verbatim(exe),
        launch_arguments: vec![
            OsString::from("observe"),
            OsString::from("run"),
            OsString::from("--service"),
            OsString::from("--data-dir"),
            data_dir.clone().into_os_string(),
        ],
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(
            &info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(describe)?;
    service
        .set_description(
            "Bounded, privacy-preserving workload observation for an explicitly initialized eidos study. Local only; no listener or upload.",
        )
        .map_err(describe)?;
    service.set_delayed_auto_start(true).map_err(describe)?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                };
                3
            ]),
        })
        .map_err(describe)?;
    println!("installed service {SERVICE_NAME} (LocalSystem, delayed auto start)");
    println!("data directory: {}", data_dir.display());
    if start_now {
        service.start::<&str>(&[]).map_err(describe)?;
        wait_for(&service, ServiceState::Running, Duration::from_secs(60))?;
        println!("started");
    }
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(describe)?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            println!("service {SERVICE_NAME} is not installed");
            return Ok(());
        }
        Err(error) => return Err(describe(error)),
    };
    if service.query_status().map_err(describe)?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        wait_for(&service, ServiceState::Stopped, Duration::from_secs(120))?;
    }
    service.delete().map_err(describe)?;
    println!("removed service {SERVICE_NAME}; the data directory and study key are kept");
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    let service = open(ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    if service.query_status().map_err(describe)?.current_state == ServiceState::Running {
        println!("already running");
        return Ok(());
    }
    service.start::<&str>(&[]).map_err(describe)?;
    wait_for(&service, ServiceState::Running, Duration::from_secs(60))?;
    println!("started");
    Ok(())
}

pub fn stop() -> anyhow::Result<()> {
    let service = open(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    if service.query_status().map_err(describe)?.current_state == ServiceState::Stopped {
        println!("already stopped");
        return Ok(());
    }
    service.stop().map_err(describe)?;
    wait_for(&service, ServiceState::Stopped, Duration::from_secs(120))?;
    println!("stopped");
    Ok(())
}

pub fn registration() -> anyhow::Result<Option<String>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(describe)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => {
            let status = service.query_status().map_err(describe)?;
            Ok(Some(format!("{:?}", status.current_state)))
        }
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
        Err(error) => Err(describe(error)),
    }
}

fn open(access: ServiceAccess) -> anyhow::Result<windows_service::service::Service> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(describe)?;
    manager.open_service(SERVICE_NAME, access).map_err(|error| {
        if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
            anyhow::anyhow!("service {SERVICE_NAME} is not installed; run `eidos observe install`")
        } else {
            describe(error)
        }
    })
}

fn wait_for(
    service: &windows_service::service::Service,
    wanted: ServiceState,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = service.query_status().map_err(describe)?;
        if status.current_state == wanted {
            return Ok(());
        }
        if status.current_state == ServiceState::Stopped && wanted == ServiceState::Running {
            bail!(
                "service stopped during start ({:?}); see the log directory",
                status.exit_code
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for {wanted:?}; last state {:?}",
                status.current_state
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// SYSTEM and Administrators only; the spool holds tokenised but still
/// detailed traces and the DPAPI key blob.
fn restrict_acl(dir: &Path) -> anyhow::Result<()> {
    let output = std::process::Command::new("icacls")
        .arg(dir)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .output()
        .context("running icacls")?;
    if !output.status.success() {
        bail!(
            "icacls failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ----- the service itself ----------------------------------------------------

struct RunContext {
    data_dir: PathBuf,
}

static RUN: OnceLock<RunContext> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Entry point for `eidos observe run --service`; must be started by the SCM.
pub fn run_service(data_dir: PathBuf) -> anyhow::Result<()> {
    let _ = RUN.set(RunContext { data_dir });
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(()),
        Err(error) if os_error(&error) == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) => bail!(
            "`eidos observe run --service` is the service entry point and must be started by the service control manager; use `eidos observe run` for the foreground or `eidos observe start`"
        ),
        Err(error) => Err(describe(error)),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let context = RUN.get().expect("run context set before dispatch");
    let (control_tx, control_rx) = mpsc::channel::<ControlEvent>();
    let control_tx = Mutex::new(Some(control_tx));
    let status_slot: std::sync::Arc<Mutex<Option<service_control_handler::ServiceStatusHandle>>> =
        std::sync::Arc::new(Mutex::new(None));
    let handler = {
        let status_slot = status_slot.clone();
        move |control: ServiceControl| {
            let send = |event: ControlEvent| {
                if let Some(tx) = control_tx.lock().unwrap().as_ref() {
                    let _ = tx.send(event);
                }
            };
            match control {
                ServiceControl::Stop => {
                    if let Some(handle) = *status_slot.lock().unwrap() {
                        let _ = handle.set_service_status(status(ServiceState::StopPending, 0, 60));
                    }
                    send(ControlEvent::Stop);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                    if let Some(handle) = *status_slot.lock().unwrap() {
                        let _ = handle.set_service_status(status(ServiceState::StopPending, 0, 60));
                    }
                    send(ControlEvent::Shutdown);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::PowerEvent(param) => {
                    match param {
                        PowerEventParam::Suspend => send(ControlEvent::Suspend),
                        PowerEventParam::ResumeAutomatic | PowerEventParam::ResumeSuspend => {
                            send(ControlEvent::Resume)
                        }
                        PowerEventParam::PowerStatusChange | PowerEventParam::BatteryLow => {
                            send(ControlEvent::PowerStatusChange)
                        }
                        _ => {}
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        }
    };
    let handle = match service_control_handler::register(SERVICE_NAME, handler) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::error!(error = %error, "cannot register the service control handler");
            return;
        }
    };
    *status_slot.lock().unwrap() = Some(handle);
    let _ = handle.set_service_status(status(ServiceState::StartPending, 1, 60));

    let on_ready = move || {
        let _ = handle.set_service_status(status(ServiceState::Running, 0, 0));
        tracing::info!("collector running");
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        daemon::run(
            Options {
                data_dir: context.data_dir.clone(),
            },
            control_rx,
            on_ready,
        )
    }));
    let exit = match result {
        Ok(Ok(())) => ServiceExitCode::Win32(0),
        Ok(Err(error)) => {
            tracing::error!(error = %format!("{error:#}"), "collector failed");
            ServiceExitCode::ServiceSpecific(1)
        }
        Err(_) => {
            tracing::error!("collector panicked");
            ServiceExitCode::ServiceSpecific(2)
        }
    };
    let mut stopped = status(ServiceState::Stopped, 0, 0);
    stopped.exit_code = exit;
    let _ = handle.set_service_status(stopped);
}

fn status(state: ServiceState, checkpoint: u32, wait_hint_s: u64) -> ServiceStatus {
    let controls = if state == ServiceState::Running {
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::PRESHUTDOWN
            | ServiceControlAccept::POWER_EVENT
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

// ----- helpers ----------------------------------------------------------------

fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    Ok(strip_verbatim(std::path::absolute(path)?))
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

fn os_error(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

fn describe(error: windows_service::Error) -> anyhow::Error {
    match error {
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(5) => {
            anyhow::anyhow!("access denied; run from an elevated prompt")
        }
        other => anyhow::anyhow!("{other}"),
    }
}

/// Lets the foreground runner reuse the same control channel shape.
pub fn control_channel() -> (Sender<ControlEvent>, mpsc::Receiver<ControlEvent>) {
    mpsc::channel()
}
