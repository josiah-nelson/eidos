//! Release update check: once at start-up and then daily, ask GitHub for
//! the latest release tag and remember whether it is newer than this build.
//!
//! Read-only and advisory: nothing downloads or installs anything here.
//! Pushed updates are a separate, signed mechanism — a node must verify a
//! new binary's Authenticode signature against the pinned release
//! certificate before executing it, and this module deliberately stays on
//! the safe side of that line.

use crate::state::AppState;
use std::sync::Arc;
use std::time::Duration;

const RELEASES_LATEST: &str = "https://api.github.com/repos/josiah-nelson/eidos/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// First check shortly after start so the UI is honest without a day's wait,
/// but off the startup path (scans and recovery come first).
const FIRST_DELAY: Duration = Duration::from_secs(60);

/// `Some(tag)` when `tag` (e.g. `v0.6.1`) is newer than `current` (e.g.
/// `0.5.0`). Non-numeric tags never report an update.
pub fn newer_release(current: &str, tag: &str) -> Option<String> {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let v = v.trim().trim_start_matches('v');
        let parts: Vec<u64> = v
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        (!parts.is_empty()).then_some(parts)
    };
    let cur = parse(current)?;
    let new = parse(tag)?;
    // Compare positionally; a missing component is zero (1.2 == 1.2.0).
    let len = cur.len().max(new.len());
    for i in 0..len {
        let c = cur.get(i).copied().unwrap_or(0);
        let n = new.get(i).copied().unwrap_or(0);
        if n != c {
            return (n > c).then(|| tag.trim().to_string());
        }
    }
    None
}

fn check_once() -> anyhow::Result<Option<String>> {
    check_url(RELEASES_LATEST, Duration::from_secs(15))
}

fn check_url(url: &str, timeout: Duration) -> anyhow::Result<Option<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout.min(Duration::from_secs(5))))
        .build()
        .into();
    let body: serde_json::Value = agent
        .get(url)
        // GitHub rejects requests without a user agent.
        .header("user-agent", concat!("eidos/", env!("CARGO_PKG_VERSION")))
        .header("accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_json()?;
    let tag = body["tag_name"].as_str().unwrap_or_default();
    Ok(newer_release(env!("CARGO_PKG_VERSION"), tag))
}

/// Spawn the daily checker. The result lands in
/// [`AppState::update_available`]; failures are logged at debug (an offline
/// host must not fill its own logs with reminders that it is offline).
pub fn spawn_update_check(state: &Arc<AppState>) {
    let st = state.clone();
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let mut delay = FIRST_DELAY;
            loop {
                let mut waited = Duration::ZERO;
                // Sleep in slices so shutdown is honored promptly.
                while waited < delay {
                    if st.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    let slice = Duration::from_secs(1).min(delay - waited);
                    std::thread::sleep(slice);
                    waited += slice;
                }
                match check_once() {
                    Ok(newer) => {
                        if let Some(tag) = &newer {
                            tracing::info!(release = %tag, "a newer eidos release is available");
                        }
                        *st.update_available.lock() = newer;
                    }
                    Err(e) => tracing::debug!(error = %e, "release check failed"),
                }
                delay = CHECK_INTERVAL;
            }
        })
        .expect("spawn update check");
}

#[cfg(test)]
mod tests {
    use super::newer_release;

    #[test]
    fn response_body_stall_obeys_the_global_deadline() {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "release-check client never connected"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).unwrap();
            // Valid headers, but never finish the body until the caller returns.
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{").unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(5));
        });
        let start = Instant::now();
        let result = super::check_url(&format!("http://{address}"), Duration::from_millis(150));
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "body read did not time out"
        );
    }

    #[test]
    fn version_comparison_is_numeric_and_v_prefix_tolerant() {
        assert_eq!(newer_release("0.5.0", "v0.5.1"), Some("v0.5.1".into()));
        assert_eq!(newer_release("0.5.0", "v0.10.0"), Some("v0.10.0".into()));
        assert_eq!(newer_release("0.5.0", "v0.5.0"), None);
        assert_eq!(newer_release("0.5.1", "v0.5.0"), None);
        assert_eq!(newer_release("0.5.0", "v1.0"), Some("v1.0".into()));
        assert_eq!(
            newer_release("0.5.0", "nightly"),
            None,
            "non-numeric tags never fire"
        );
    }
}
