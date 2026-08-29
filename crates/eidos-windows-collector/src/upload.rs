//! Scheduled bundle upload.
//!
//! Once a day, at or after a configured local hour, the collector stages an
//! export and copies it to a file share, so a fleet deposits its observations
//! without anyone visiting each host.
//!
//! `destination` is an ordinary directory path — typically a UNC share such
//! as `\\fileserver\share\eidos`. The collector runs as LocalSystem, so a
//! share must grant write access to the machine account, not to the operator
//! who configured it.
//!
//! Uploads are deliberately unhurried: the scheduler checks the clock once a
//! minute, a day's upload is attempted a bounded number of times, and every
//! staged bundle that has not yet been copied is retried on the next run, so a
//! share that is offline for a day catches up rather than losing that day.
//!
//! The copying itself runs on a detached thread. A stalled SMB operation is
//! not interruptible, and the daemon joins its lane threads on shutdown, so
//! the supervised thread here only ever watches the clock — it must never be
//! the thread sitting inside a network filesystem call when the service is
//! asked to stop.

use crate::daemon::{stage_export, Shared};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::System::SystemInformation::{
    ComputerNameDnsFullyQualified, GetComputerNameExW,
};

/// How often the scheduler looks at the clock.
const TICK: Duration = Duration::from_secs(60);

/// Bundles carry this suffix; anything else in the export directory is left
/// alone.
const BUNDLE_SUFFIX: &str = ".eidos-observation.zst";

/// True while a delivery is in flight, so a slow share cannot accumulate
/// overlapping runs.
static UPLOADING: AtomicBool = AtomicBool::new(false);

pub fn start(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("collector-upload".into())
        .spawn(move || scheduler(shared))
        .expect("spawn upload scheduler")
}

/// Where the day's delivery has got to.
#[derive(Default)]
struct Progress {
    /// The day this progress describes.
    day: Option<i64>,
    /// Attempts made today, bounded by `upload.attempts`.
    attempts: u32,
    /// Today's delivery finished with nothing left behind.
    done: bool,
}

fn scheduler(shared: Arc<Shared>) {
    let progress = Arc::new(Mutex::new(Progress::default()));
    while !shared.is_shutting_down() {
        if shared.sleep_unless_stopping(TICK) {
            return;
        }
        let settings = shared.config.lock().unwrap().upload.clone();

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

        let due = {
            let mut progress = progress.lock().unwrap();
            if progress.day != Some(today) {
                // A new day restores the attempt budget.
                *progress = Progress {
                    day: Some(today),
                    attempts: 0,
                    done: false,
                };
            }
            !progress.done
                && u32::from(now.wHour) >= settings.hour
                && progress.attempts < settings.attempts.max(1)
        };
        if !due || UPLOADING.swap(true, Ordering::AcqRel) {
            continue;
        }
        progress.lock().unwrap().attempts += 1;

        // Detached: a stalled share must not hold up an SCM stop.
        let spawned = {
            let shared = shared.clone();
            let progress = progress.clone();
            std::thread::Builder::new()
                .name("collector-upload-run".into())
                .spawn(move || {
                    deliver(&shared, &settings, &progress, today);
                    UPLOADING.store(false, Ordering::Release);
                })
        };
        if spawned.is_err() {
            UPLOADING.store(false, Ordering::Release);
        }
    }
}

fn deliver(
    shared: &Arc<Shared>,
    settings: &crate::config::UploadConfig,
    progress: &Mutex<Progress>,
    today: i64,
) {
    let (uploaded, failure) = run(shared, settings);
    let remaining = pending(&shared.export_dir).len() as u64;

    // Bundles that reached the share count whether or not their siblings did:
    // a mixed run still delivered them, and undercounting would make the
    // totals disagree with what is on the share.
    match failure {
        None => {
            // Only a run that left nothing behind finishes the day, and only
            // the day it was started for: a delivery that straddles midnight
            // must not mark the new day complete.
            let mut progress = progress.lock().unwrap();
            if progress.day == Some(today) {
                progress.done = true;
            }
            drop(progress);
            set_view(shared, |v| {
                v.last_error = None;
                v.last_upload_utc_ns = Some(shared.anchor().utc_ns);
                v.uploaded_total += uploaded;
                v.pending = remaining;
            });
            tracing::info!(uploaded, "daily upload complete");
        }
        Some(error) => {
            let message = error.to_string();
            let attempts = progress.lock().unwrap().attempts;
            tracing::error!(
                error = %message,
                attempt = attempts,
                uploaded,
                "daily upload incomplete"
            );
            set_view(shared, |v| {
                v.last_error = Some(message);
                v.uploaded_total += uploaded;
                v.pending = remaining;
                if uploaded > 0 {
                    v.last_upload_utc_ns = Some(shared.anchor().utc_ns);
                }
            });
        }
    }
}

/// Stage today's bundle, then copy every bundle still waiting in the export
/// directory — including any left behind by a previous failure.
///
/// Returns how many were delivered together with the first failure, if any.
/// A run that could not deliver everything is a failure — the backlog must be
/// retried rather than counted as a finished day — but what did land is still
/// reported, because it is on the share either way.
fn run(
    shared: &Arc<Shared>,
    settings: &crate::config::UploadConfig,
) -> (u64, Option<anyhow::Error>) {
    // Without a study key there is no per-host identity, and two keyless
    // collectors sharing a destination would each mistake the other's bundle
    // for their own already-delivered copy.
    let Some(prefix) = host_prefix(shared) else {
        return (
            0,
            Some(anyhow::anyhow!(
                "no study key or host identity: run `eidos observe init` before enabling upload"
            )),
        );
    };
    let destination = Path::new(settings.destination.trim());
    if let Err(error) = std::fs::create_dir_all(destination) {
        return (
            0,
            Some(anyhow::anyhow!(
                "cannot reach the upload destination {}: {error}",
                destination.display()
            )),
        );
    }
    if let Err(error) = stage_export(shared) {
        return (0, Some(error));
    }

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
    (uploaded, failure)
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
        // Already delivered on an earlier run whose local cleanup failed. The
        // prefix is host-specific, so this really is our own copy.
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
///
/// The study key cannot supply this on its own: a cohort deliberately shares
/// one key so content fingerprints compare across hosts, so keying a constant
/// would give every host in that cohort the same prefix — and each would then
/// mistake another's bundle for its own delivered copy. The machine's name is
/// the distinguishing input; it never leaves the host, because what reaches
/// the share is the keyed token of it.
///
/// `None` when either the key or the host identity is unavailable, which
/// leaves the collector without an identity rather than with a shared one.
fn host_prefix(shared: &Arc<Shared>) -> Option<String> {
    let host = machine_name()?;
    shared.with_key(|key| key.token("upload-host", host.as_bytes()).encoded()[..16].to_string())
}

/// The machine's fully qualified name, used only as keyed token input.
fn machine_name() -> Option<String> {
    let mut size: u32 = 0;
    // SAFETY: the first call only asks for the required buffer size.
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsFullyQualified,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: the buffer is sized as the call just requested.
    let ok = unsafe {
        GetComputerNameExW(
            ComputerNameDnsFullyQualified,
            buffer.as_mut_ptr(),
            &mut size,
        )
    };
    if ok == 0 {
        return None;
    }
    buffer.truncate(size as usize);
    let name = String::from_utf16_lossy(&buffer);
    (!name.is_empty()).then_some(name)
}

fn set_view(shared: &Shared, f: impl FnOnce(&mut crate::protocol::UploadView)) {
    f(&mut shared.upload.lock().unwrap());
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

    /// Two hosts sharing a cohort study key must still get different prefixes,
    /// or each treats the other's bundle as its own delivered copy.
    #[test]
    fn the_machine_name_separates_hosts_that_share_a_key() {
        let key = eidos_observe::StudyKey::from_bytes([7; 32]);
        let one = key.token("upload-host", b"host-one.example").encoded()[..16].to_string();
        let two = key.token("upload-host", b"host-two.example").encoded()[..16].to_string();
        assert_ne!(one, two);
        // And the share never sees the name itself.
        assert!(!one.contains("host"));
    }

    /// A backlog that is only partly delivered must not finish the day, or the
    /// bundles left behind wait until tomorrow while the status says the
    /// upload succeeded.
    #[test]
    fn a_partly_delivered_backlog_is_a_failure() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged");
        let share = temp.path().join("share");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&share).unwrap();

        let good = bundle(&staged, "observation-1-0000.eidos-observation.zst", b"ok");
        assert!(copy_one(&good, &share, "host").is_ok());

        // A directory where a bundle should be: the copy cannot succeed.
        let broken = staged.join("observation-2-0000.eidos-observation.zst");
        std::fs::create_dir_all(&broken).unwrap();
        assert!(copy_one(&broken, &share, "host").is_err());
    }
}
