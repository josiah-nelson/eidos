//! Scheduled bundle upload.
//!
//! Once a day, at or after a configured local hour, the collector stages an
//! export and copies it to a file share, so a fleet deposits its observations
//! without anyone visiting each host.
//!
//! `destination` is an ordinary directory path — typically a UNC share such
//! as `\\fileserver\share\eidos`. The service runs as LocalSystem, so a share
//! must grant write access to the machine account, not to the operator who
//! configured it.
//!
//! Uploads are deliberately unhurried: the scheduler checks the clock once a
//! minute, a day's upload is attempted a bounded number of times, and every
//! staged bundle that has not yet been copied is retried on the next run, so a
//! share that is offline for a day catches up rather than losing that day.

use crate::daemon::{stage_export, Shared};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the scheduler looks at the clock.
const TICK: Duration = Duration::from_secs(60);

/// Bundles carry this suffix; anything else in the export directory is left
/// alone.
const BUNDLE_SUFFIX: &str = ".eidos-observation.zst";

pub fn start(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("collector-upload".into())
        .spawn(move || scheduler(shared))
        .expect("spawn upload scheduler")
}

/// The day an upload last succeeded, and how many times it has been tried
/// today, so a failing share is retried a bounded number of times.
#[derive(Default)]
struct Progress {
    day: Option<i64>,
    attempts: u32,
}

fn scheduler(shared: Arc<Shared>) {
    let mut progress = Progress::default();
    while !shared.is_shutting_down() {
        if sleep_unless_stopping(&shared, TICK) {
            return;
        }
        let settings = {
            let config = shared.config.lock().unwrap();
            config.upload.clone()
        };
        // Keep the status view honest even on ticks that do nothing, so
        // `observe status` shows what is configured and what is waiting.
        let waiting = pending(&shared.export_dir).len() as u64;
        set_view(&shared, |v| {
            v.enabled = settings.enabled;
            v.destination = settings.destination.clone();
            v.pending = waiting;
        });
        if !settings.enabled || settings.destination.trim().is_empty() {
            continue;
        }
        let Some(now) = local_time() else {
            continue;
        };
        let today = day_key(&now);
        if progress.day != Some(today) {
            // A new day resets the attempt budget.
            progress = Progress {
                day: None,
                attempts: 0,
            };
        }
        if progress.day == Some(today)
            || u32::from(now.wHour) < settings.hour
            || progress.attempts >= settings.attempts.max(1)
        {
            continue;
        }
        progress.attempts += 1;
        match run(&shared, &settings) {
            Ok(uploaded) => {
                progress.day = Some(today);
                let remaining = pending(&shared.export_dir).len() as u64;
                set_view(&shared, |v| {
                    v.last_error = None;
                    v.last_upload_utc_ns = Some(shared.anchor().utc_ns);
                    v.uploaded_total += uploaded;
                    v.pending = remaining;
                });
                tracing::info!(uploaded, "daily upload complete");
            }
            Err(error) => {
                let message = error.to_string();
                tracing::error!(
                    error = %message,
                    attempt = progress.attempts,
                    "daily upload failed"
                );
                set_view(&shared, |v| v.last_error = Some(message));
            }
        }
    }
}

/// Stage today's bundle, then copy every bundle still waiting in the export
/// directory — including any left behind by a previous failure.
fn run(shared: &Arc<Shared>, settings: &crate::config::UploadConfig) -> anyhow::Result<u64> {
    let destination = Path::new(settings.destination.trim());
    std::fs::create_dir_all(destination).map_err(|error| {
        anyhow::anyhow!(
            "cannot reach the upload destination {}: {error}",
            destination.display()
        )
    })?;
    stage_export(shared)?;

    let prefix = host_prefix(shared);
    let mut uploaded = 0;
    let mut failure = None;
    for bundle in pending(&shared.export_dir) {
        if shared.is_shutting_down() {
            break;
        }
        match copy_one(&bundle, destination, &prefix) {
            Ok(()) => {
                uploaded += 1;
                if settings.remove_after_upload {
                    if let Err(error) = std::fs::remove_file(&bundle) {
                        tracing::warn!(
                            error = %error,
                            file = %bundle.display(),
                            "uploaded bundle could not be removed locally"
                        );
                    }
                }
            }
            // Keep going: one unreadable bundle should not strand the rest.
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        Some(error) if uploaded == 0 => Err(error),
        _ => Ok(uploaded),
    }
}

/// Bundles waiting in the export directory, oldest first.
fn pending(export_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(export_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(BUNDLE_SUFFIX))
        })
        .collect();
    files.sort();
    files
}

/// Copy through a temporary name so a reader on the share never sees a
/// half-written bundle, and so an interrupted upload leaves nothing that
/// looks complete.
fn copy_one(bundle: &Path, destination: &Path, prefix: &str) -> anyhow::Result<()> {
    let name = bundle
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("unreadable bundle name"))?;
    let final_path = destination.join(format!("{prefix}-{name}"));
    if final_path.exists() {
        // Already delivered on an earlier run whose local cleanup failed.
        return Ok(());
    }
    let temporary = destination.join(format!("{prefix}-{name}.part"));
    std::fs::copy(bundle, &temporary).map_err(|error| {
        anyhow::anyhow!("copying {} to {}: {error}", name, destination.display())
    })?;
    std::fs::rename(&temporary, &final_path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        anyhow::anyhow!("publishing {} on {}: {error}", name, destination.display())
    })?;
    Ok(())
}

/// A stable, keyed per-host prefix so several collectors can share one
/// directory without colliding, and without putting a host name on the share.
fn host_prefix(shared: &Arc<Shared>) -> String {
    shared
        .with_key(|key| key.token("upload-host", b"collector").encoded()[..16].to_string())
        .unwrap_or_else(|| "unkeyed".into())
}

fn set_view(shared: &Shared, f: impl FnOnce(&mut crate::protocol::UploadView)) {
    f(&mut shared.upload.lock().unwrap());
}

/// Sleep in short slices so shutdown is not delayed by a whole tick. Returns
/// true when the collector is stopping.
fn sleep_unless_stopping(shared: &Shared, total: Duration) -> bool {
    let slice = Duration::from_millis(250);
    let mut slept = Duration::ZERO;
    while slept < total {
        if shared.is_shutting_down() {
            return true;
        }
        std::thread::sleep(slice);
        slept += slice;
    }
    shared.is_shutting_down()
}

fn day_key(now: &windows_sys::Win32::Foundation::SYSTEMTIME) -> i64 {
    now.wYear as i64 * 10_000 + now.wMonth as i64 * 100 + now.wDay as i64
}

fn local_time() -> Option<windows_sys::Win32::Foundation::SYSTEMTIME> {
    // SAFETY: the struct is zeroed and filled by the call.
    let mut now: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now) };
    (now.wYear != 0).then_some(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn only_bundles_are_pending_and_they_are_ordered() {
        let temp = tempfile::tempdir().unwrap();
        bundle(
            temp.path(),
            "observation-200-0001.eidos-observation.zst",
            b"b",
        );
        bundle(
            temp.path(),
            "observation-100-0001.eidos-observation.zst",
            b"a",
        );
        // Neither of these is a finished bundle.
        bundle(temp.path(), "collector.log", b"noise");
        bundle(
            temp.path(),
            "observation-300.eidos-observation.zst.part",
            b"x",
        );

        let found = pending(temp.path());
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "observation-100-0001.eidos-observation.zst",
                "observation-200-0001.eidos-observation.zst",
            ]
        );
    }

    #[test]
    fn copy_publishes_atomically_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged");
        let share = temp.path().join("share");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&share).unwrap();
        let file = bundle(
            &staged,
            "observation-1-0000.eidos-observation.zst",
            b"payload",
        );

        copy_one(&file, &share, "hosttoken").unwrap();
        let delivered = share.join("hosttoken-observation-1-0000.eidos-observation.zst");
        assert_eq!(std::fs::read(&delivered).unwrap(), b"payload");
        // Nothing half-written is left behind under the temporary name.
        assert!(!share
            .join("hosttoken-observation-1-0000.eidos-observation.zst.part")
            .exists());

        // A bundle whose local cleanup failed is not copied twice, and the
        // delivered copy is left exactly as it was.
        std::fs::write(&file, b"different").unwrap();
        copy_one(&file, &share, "hosttoken").unwrap();
        assert_eq!(std::fs::read(&delivered).unwrap(), b"payload");
    }

    #[test]
    fn the_host_prefix_keeps_two_collectors_apart() {
        let temp = tempfile::tempdir().unwrap();
        let share = temp.path().join("share");
        std::fs::create_dir_all(&share).unwrap();
        let file = bundle(
            temp.path(),
            "observation-1-0000.eidos-observation.zst",
            b"p",
        );

        copy_one(&file, &share, "aaaaaaaaaaaaaaaa").unwrap();
        copy_one(&file, &share, "bbbbbbbbbbbbbbbb").unwrap();
        assert_eq!(pending(&share).len(), 2);
    }
}
