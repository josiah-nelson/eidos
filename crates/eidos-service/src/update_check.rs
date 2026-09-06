//! Periodic driver for the durable release advisory in [`crate::updates`].

use crate::state::AppState;
use std::sync::Arc;
use std::time::Duration;

const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How often a node whose operator turned automatic checks off re-reads that
/// setting, so turning it back on takes effect without restarting the service.
const DISABLED_INTERVAL: Duration = Duration::from_secs(60);
/// First check shortly after start so the UI is honest without a day's wait,
/// but off the startup path (scans and recovery come first).
const FIRST_DELAY: Duration = Duration::from_secs(60);

/// Spawn the daily checker. The result lands in
/// [`AppState::updates`]; failures are logged at debug (an offline
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
                // Read the durable setting every pass: a node that reported
                // `checks_enabled: false` must not still be checking daily.
                if !st.updates.checks_enabled() {
                    delay = DISABLED_INTERVAL;
                    continue;
                }
                match st.updates.check() {
                    Ok(state) => if let Some(release) = state.available {
                        tracing::info!(release = %release.version, "a newer eidos release is available");
                    },
                    Err(e) => tracing::debug!(error = %e, "release check failed"),
                }
                delay = CHECK_INTERVAL;
            }
        })
        .expect("spawn update check");
}
