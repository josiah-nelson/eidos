//! L2: read-only enumeration of a fixed volume with the production lister,
//! timed and summarised. Scheduled every `every_hours` when enabled, or run
//! once on request. It lists directories only; it opens no file.

use crate::daemon::Shared;
use crate::resources;
use crate::volumes::VolumeFacts;
use eidos_domain::FileAttributes;
use eidos_observe::{
    bucket_depth, bucket_extension, bucket_size, DepthBucket, EnumerationProbe, ExtensionBucket,
    Histogram, ObservationRecord, SizeBucket,
};
use eidos_scanner::default_lister;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_DEPTH: usize = 128;
const MAX_DURATION: Duration = Duration::from_secs(4 * 3600);

pub fn start(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("enumeration-scheduler".into())
        .spawn(move || scheduler(shared))
        .expect("spawn enumeration scheduler")
}

fn scheduler(shared: Arc<Shared>) {
    // First scheduled run waits a full period so an install does not walk
    // every volume immediately; `observe probe` exists for that.
    let mut last_run = Instant::now();
    while !shared.is_shutting_down() {
        std::thread::sleep(Duration::from_secs(30));
        let (enabled, every_hours) = {
            let config = shared.config.lock().unwrap();
            (
                config.lanes.enumeration.enabled,
                config.lanes.enumeration.every_hours.max(1),
            )
        };
        if !enabled {
            last_run = Instant::now();
            continue;
        }
        if last_run.elapsed() >= Duration::from_secs(every_hours as u64 * 3600) {
            last_run = Instant::now();
            if RUNNING.swap(true, Ordering::AcqRel) {
                // An on-demand probe is already walking; let it stand in for
                // this scheduled run rather than doubling the load.
                continue;
            }
            run_all(&shared, None);
            RUNNING.store(false, Ordering::Release);
        }
    }
}

/// True while a probe is walking, so repeated requests cannot multiply the
/// scans. A full-volume walk is the most expensive thing the collector does,
/// and overlapping walks contend for the same disk and distort each other's
/// measurements.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Run probes now on a background thread; `volume` narrows to one root.
/// A request that arrives while a probe is already running is declined rather
/// than queued: the caller wants a current picture, and one is already coming.
pub fn run_detached(shared: Arc<Shared>, volume: Option<String>) -> bool {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("enumeration-probe".into())
        .spawn(move || {
            run_all(&shared, volume.as_deref());
            RUNNING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::Release);
        return false;
    }
    true
}

fn run_all(shared: &Shared, only: Option<&str>) {
    let excluded = shared.config.lock().unwrap().exclude_volumes.clone();
    let targets: Vec<VolumeFacts> = shared
        .volumes
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v.drive == eidos_observe::DriveKind::Fixed)
        .filter(|v| !excluded.iter().any(|e| v.matches_exclusion(e)))
        .filter(|v| only.is_none_or(|wanted| v.matches_exclusion(wanted)))
        .cloned()
        .collect();
    if targets.is_empty() {
        tracing::warn!("enumeration probe: no matching fixed volume");
    }
    for volume in targets {
        if shared.is_shutting_down() {
            break;
        }
        let root = PathBuf::from(volume.root());
        tracing::info!(root = %volume.root(), "enumeration probe started");
        let probe = walk(shared, &volume, &root);
        tracing::info!(
            root = %volume.root(),
            files = probe.files,
            directories = probe.directories,
            errors = probe.errors,
            ms = probe.duration_ms,
            "enumeration probe finished"
        );
        shared.append(ObservationRecord::Enumeration(probe));
    }
}

fn walk(shared: &Shared, volume: &VolumeFacts, root: &std::path::Path) -> EnumerationProbe {
    let lister = default_lister();
    let started = Instant::now();
    let cpu_before = resources::sample_process().cpu_ms;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut files = 0u64;
    let mut directories = 0u64;
    let mut errors = 0u64;
    let mut max_depth = 0usize;
    let mut fan_out = Histogram::new();
    let mut sizes: BTreeMap<SizeBucket, u64> = BTreeMap::new();
    let mut extensions: BTreeMap<ExtensionBucket, u64> = BTreeMap::new();
    let (
        mut reparse_points,
        mut placeholders,
        mut sparse,
        mut compressed,
        mut encrypted,
        mut offline,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut under_allocated = 0u64;
    while let Some((directory, depth)) = stack.pop() {
        if shared.is_shutting_down() || started.elapsed() > MAX_DURATION {
            errors += 1;
            break;
        }
        let entries = match lister.list(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        fan_out.observe(entries.len() as u64);
        max_depth = max_depth.max(depth);
        for entry in entries {
            let attributes = entry.attributes;
            if attributes.is_reparse() {
                reparse_points += 1;
                if is_cloud_tag(entry.reparse_tag) {
                    placeholders += 1;
                }
            } else if attributes.has(FileAttributes::RECALL_ON_DATA_ACCESS) {
                placeholders += 1;
            }
            if attributes.has(FileAttributes::SPARSE) {
                sparse += 1;
            }
            if attributes.has(FileAttributes::COMPRESSED) {
                compressed += 1;
            }
            if attributes.has(FileAttributes::ENCRYPTED) {
                encrypted += 1;
            }
            if attributes.has(FileAttributes::OFFLINE) {
                offline += 1;
            }
            if entry.is_dir() {
                directories += 1;
                if entry.is_traversable_dir() && depth < MAX_DEPTH {
                    stack.push((directory.join(&entry.name), depth + 1));
                }
            } else {
                files += 1;
                *sizes.entry(bucket_size(entry.size)).or_default() += 1;
                let extension = entry.name.rsplit_once('.').map(|(_, ext)| ext);
                *extensions.entry(bucket_extension(extension)).or_default() += 1;
                if entry
                    .allocated
                    .is_some_and(|allocated| allocated < entry.size)
                {
                    under_allocated += 1;
                }
            }
        }
    }
    let cpu_after = resources::sample_process().cpu_ms;
    EnumerationProbe {
        at: shared.anchor(),
        volume: shared
            .with_key(|key| volume.token(key))
            .unwrap_or_else(|| eidos_observe::StudyKey::from_bytes([0; 32]).token("volume", b"")),
        duration_ms: started.elapsed().as_millis() as u64,
        cpu_ms: cpu_after.saturating_sub(cpu_before),
        files,
        directories,
        errors,
        max_depth: if max_depth == 0 {
            DepthBucket::Root
        } else {
            bucket_depth(max_depth)
        },
        fan_out,
        sizes: sizes.into_iter().collect(),
        extensions: extensions.into_iter().collect(),
        reparse_points,
        placeholders,
        sparse,
        compressed,
        encrypted,
        offline,
        hard_linked: 0,
        under_allocated,
    }
}

/// Cloud-files reparse tags share the 0x9000xxxx range (IO_REPARSE_TAG_CLOUD
/// and its per-provider variants).
fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0000 == 0x9000_0000
}

#[cfg(test)]
mod tests {
    #[test]
    fn cloud_tags_are_recognised() {
        assert!(super::is_cloud_tag(0x9000_001A));
        assert!(super::is_cloud_tag(0x9000_101A));
        assert!(!super::is_cloud_tag(0xA000_000C));
    }
}
