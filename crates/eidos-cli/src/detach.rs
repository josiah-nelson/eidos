//! `eidos serve --detach`: run the service as a background process for the
//! current user (no console window, logs in `--log-dir`), the per-user
//! counterpart of the Windows service.
//!
//! The parent re-launches itself without `--detach`, waits until the API
//! answers, prints the URL, and exits. If something already answers on the
//! address (a previous start, or the service), nothing is launched.

use crate::ServeArgs;
use anyhow::{bail, Context};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn start(args: &ServeArgs) -> anyhow::Result<()> {
    let url = format!("http://{}/", browse_addr(args));
    if probe(&url, Duration::from_millis(500)) {
        println!("eidos is already running at {url}");
        return Ok(());
    }

    let exe = std::env::current_exe().context("locating eidos.exe")?;
    let mut child = args.clone();
    child.detach = false;
    if child.log_dir.is_none() {
        child.log_dir = Some(child.data_dir.join("logs"));
    }
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .args(child.to_command_line())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // Breakaway so the installer's job object does not take the
        // service down with it when setup exits; harmless elsewhere.
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
    }
    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(5) => {
            // ERROR_ACCESS_DENIED: breakaway not permitted by the job.
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0008);
            cmd.spawn().context("starting eidos in the background")?
        }
        Err(e) => return Err(e).context("starting eidos in the background"),
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if probe(&url, Duration::from_millis(500)) {
            println!("eidos is running at {url} (pid {})", proc.id());
            let log_dir = child.log_dir.as_deref().unwrap_or(&child.data_dir);
            println!("logs: {}", log_dir.display());
            return Ok(());
        }
        if let Some(status) = proc.try_wait()? {
            bail!(
                "eidos exited during start-up ({status}); see the log in {}",
                child
                    .log_dir
                    .as_deref()
                    .unwrap_or(&child.data_dir)
                    .display()
            );
        }
        if Instant::now() > deadline {
            bail!("eidos did not answer at {url} within 60 s; it is still starting, see the log directory");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Address a local client can reach: a wildcard bind is reached on loopback.
fn browse_addr(args: &ServeArgs) -> String {
    if args.bind.ip().is_unspecified() {
        format!("127.0.0.1:{}", args.bind.port())
    } else {
        args.bind.to_string()
    }
}

fn probe(url: &str, timeout: Duration) -> bool {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .new_agent();
    agent
        .get(format!("{url}api/health"))
        .call()
        .map(|r| r.status().as_u16() == 200)
        .unwrap_or(false)
}
