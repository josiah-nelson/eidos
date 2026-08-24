//! Windows service lifecycle against the real service control manager.
//!
//! Registers a throwaway service (`eidos-test-<pid>`) that points at a temp
//! data directory, then drives install → start → status → restart → stop →
//! uninstall through the `eidos service` verbs and checks that catalog state
//! survives the restart and that nothing is left registered afterwards.
//!
//! Needs an elevated session and opt-in: `EIDOS_SCM_TESTS=1`. Otherwise the
//! test reports itself skipped and passes, like the USN-journal tests.

#![cfg(windows)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

const EXE: &str = env!("CARGO_BIN_EXE_eidos");

struct Svc {
    name: String,
}

impl Svc {
    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(EXE)
            .arg("service")
            .arg("--name")
            .arg(&self.name)
            .args(args)
            .output()
            .expect("run eidos");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }
    fn ok(&self, args: &[&str]) -> String {
        let (ok, text) = self.run(args);
        assert!(ok, "eidos service {} failed:\n{text}", args.join(" "));
        text
    }
}

impl Drop for Svc {
    fn drop(&mut self) {
        // Never leave a registration behind, even when an assertion failed.
        let _ = self.run(&["uninstall", "--timeout", "30"]);
    }
}

fn elevated() -> bool {
    Command::new("net")
        .args(["session"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(url: &str) -> (u16, String, Vec<(String, String)>) {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    let mut resp = agent.get(url).call().expect("http");
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    (status, body, headers)
}

#[test]
fn service_lifecycle_preserves_state() {
    if std::env::var("EIDOS_SCM_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set EIDOS_SCM_TESTS=1 in an elevated session to run");
        return;
    }
    assert!(elevated(), "EIDOS_SCM_TESTS=1 requires an elevated session");

    let dir = tempfile::tempdir().unwrap();
    // A space in the path checks command-line quoting on the registration.
    let data = dir.path().join("eidos data");
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let svc = Svc {
        name: format!("eidos-test-{}", std::process::id()),
    };

    let out = svc.ok(&[
        "install",
        "--data-dir",
        data.to_str().unwrap(),
        "--bind",
        &bind,
        "--start",
        "manual",
        "--no-content",
    ]);
    assert!(out.contains("installed service"), "{out}");
    assert!(data.join("logs").is_dir(), "log dir created at install");

    // Not started yet: status is friendly, not a raw Win32 code.
    let out = svc.ok(&["status"]);
    assert!(
        out.contains("state:       stopped (not started since boot)"),
        "{out}"
    );
    assert!(
        out.contains(&format!("url:         http://{bind}/")),
        "{out}"
    );

    svc.ok(&["start"]);
    let (code, body, _) = get(&format!("http://{bind}/api/health"));
    assert_eq!(code, 200, "{body}");
    let first_pid = pid_of(&svc.ok(&["status"]));

    // Web UI is served from the executable (when this build embeds it).
    let (code, index, headers) = get(&format!("http://{bind}/"));
    let embedded = headers
        .iter()
        .any(|(k, v)| k == "content-type" && v.starts_with("text/html"));
    if embedded {
        assert_eq!(code, 200);
        assert!(index.contains("<html"), "index served: {index:.80}");
        let (deep, _, _) = get(&format!("http://{bind}/search/anything"));
        assert_eq!(deep, 200, "SPA deep link falls back to index");
    }

    // Restart: catalog file persists and a new process serves it.
    assert!(data.join("catalog.db").is_file());
    svc.ok(&["restart"]);
    let second_pid = pid_of(&svc.ok(&["status"]));
    assert_ne!(first_pid, second_pid, "restart spawned a new process");
    assert!(
        data.join("catalog.db").is_file(),
        "catalog retained across restart"
    );
    let (code, _, _) = get(&format!("http://{bind}/api/health"));
    assert_eq!(code, 200);

    svc.ok(&["stop"]);
    let out = svc.ok(&["status"]);
    assert!(
        out.contains("state:       stopped (exit 0)"),
        "clean stop: {out}"
    );
    // Starting an already-installed, stopped service again also works.
    svc.ok(&["start"]);
    svc.ok(&["stop"]);

    let out = svc.ok(&["uninstall"]);
    assert!(out.contains("indexed data kept in"), "{out}");
    assert!(
        data.join("catalog.db").is_file(),
        "uninstall never deletes data"
    );
    let (ok, text) = svc.run(&["status"]);
    assert!(!ok && text.contains("not installed"), "{text}");

    // The service's log names the data directory and the running URL.
    let log = latest_log(&data.join("logs"));
    assert!(log.contains("service starting"), "{log:.300}");
    assert!(log.contains("service running"), "{log:.300}");
    assert!(log.contains("service stopped"), "{log:.300}");
}

fn pid_of(status: &str) -> u32 {
    let line = status
        .lines()
        .find(|l| l.starts_with("state:") && l.contains("running"))
        .unwrap_or_else(|| panic!("not running:\n{status}"));
    line.split("pid ")
        .nth(1)
        .and_then(|s| s.trim_end_matches(')').parse().ok())
        .expect("pid")
}

fn latest_log(dir: &Path) -> String {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    files.sort();
    std::fs::read_to_string(files.last().expect("a log file")).unwrap()
}
