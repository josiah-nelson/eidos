//! L1: one reader per journaled local volume. Reads the USN journal with
//! the scanner's cancellable wait, feeds records through the analyzer, and
//! spools logical changes in batches. The cursor (journal id, next USN) is
//! persisted per volume so a restart resumes exactly; overflow and journal
//! recreation become capture gaps rather than silent resets.

use crate::analytics::{ChangeAnalyzer, ObjectFacts, RecordView};
use crate::daemon::{FeedStatus, Shared};
use crate::volumes::VolumeFacts;
use eidos_observe::{
    bucket_size, FeedCursor, FeedHealthRecord, FeedKind, FeedState, GapCause, ObservationRecord,
    PercentBucket,
};
use eidos_scanner::usn::{
    query_journal, read_journal_wait_timeout, JournalCancellation, ReadOutcome, UsnError,
    VolumeHandle,
};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Above this many records in one batch the size/depth lookups are skipped
/// so a burst cannot turn into excessive parent-directory enumeration.
const FACT_LOOKUP_BATCH_LIMIT: usize = 2_000;
const READ_BUFFER: usize = 1024 * 1024;
const CURSOR_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    journal_id: u64,
    next_usn: i64,
}

impl Cursor {
    fn feed_cursor(&self) -> FeedCursor {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.journal_id.to_le_bytes());
        bytes[8..].copy_from_slice(&self.next_usn.to_le_bytes());
        FeedCursor {
            feed: FeedKind::Usn,
            version: CURSOR_VERSION,
            opaque: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

fn cursor_path(shared: &Shared, volume: &VolumeFacts) -> PathBuf {
    let id = blake3::hash(volume.guid_path.as_bytes()).to_hex();
    shared.data_dir.join(format!("usn-{}.cursor", &id[..16]))
}

fn load_cursor(path: &std::path::Path) -> Option<Cursor> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save_cursor(path: &std::path::Path, cursor: &Cursor) {
    let temporary = path.with_extension("cursor.tmp");
    if std::fs::write(&temporary, serde_json::to_vec(cursor).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

struct Reader {
    cancel: Arc<JournalCancellation>,
    thread: JoinHandle<()>,
}

/// Supervisor: keeps one reader per eligible volume while the lane is on.
pub fn start(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("usn-supervisor".into())
        .spawn(move || {
            let mut readers: HashMap<String, Reader> = HashMap::new();
            loop {
                if shared.is_shutting_down() {
                    break;
                }
                let enabled = shared.lane_enabled(|c| c.lanes.usn);
                let excluded = shared.config.lock().unwrap().exclude_volumes.clone();
                let wanted: Vec<VolumeFacts> = if enabled {
                    shared
                        .volumes
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|v| v.is_feed_candidate())
                        .filter(|v| !excluded.iter().any(|e| v.matches_exclusion(e)))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                // Stop readers whose volume left or whose thread died.
                let stale: Vec<String> = readers
                    .iter()
                    .filter(|(guid, reader)| {
                        reader.thread.is_finished() || !wanted.iter().any(|v| &v.guid_path == *guid)
                    })
                    .map(|(guid, _)| guid.clone())
                    .collect();
                for guid in stale {
                    if let Some(reader) = readers.remove(&guid) {
                        reader.cancel.cancel();
                        let _ = reader.thread.join();
                    }
                }
                for volume in wanted {
                    if readers.contains_key(&volume.guid_path) {
                        continue;
                    }
                    let Ok(cancel) = JournalCancellation::new().map(Arc::new) else {
                        continue;
                    };
                    let shared = shared.clone();
                    let root = volume.root().to_string();
                    let guid = volume.guid_path.clone();
                    let thread_cancel = cancel.clone();
                    let thread = std::thread::Builder::new()
                        .name(format!("usn-{}", root.trim_end_matches('\\')))
                        .spawn(move || read_volume(shared, volume, thread_cancel));
                    if let Ok(thread) = thread {
                        readers.insert(guid, Reader { cancel, thread });
                    }
                }
                if shared.sleep_unless_stopping(Duration::from_secs(5)) {
                    break;
                }
            }
            for (_, reader) in readers.drain() {
                reader.cancel.cancel();
                let _ = reader.thread.join();
            }
        })
        .expect("spawn usn supervisor")
}

fn set_status(shared: &Shared, volume: &VolumeFacts, f: impl FnOnce(&mut FeedStatus)) {
    let mut feeds = shared.feeds.lock().unwrap();
    let status = feeds
        .entry(volume.guid_path.clone())
        .or_insert_with(|| FeedStatus::new(volume.root().to_string()));
    f(status);
}

fn read_volume(shared: Arc<Shared>, volume: VolumeFacts, cancel: Arc<JournalCancellation>) {
    let cursor_file = cursor_path(&shared, &volume);
    let now_s = crate::daemon::utc_now_ns() / 1_000_000_000;
    let mut analyzer = ChangeAnalyzer::new(volume.guid_path.as_bytes(), now_s);
    let mut size_cache: LruCache<u128, u64> = LruCache::new(NonZeroUsize::new(65_536).unwrap());
    let mut depth_cache: LruCache<u128, usize> = LruCache::new(NonZeroUsize::new(65_536).unwrap());
    let mut buffer = vec![0u8; READ_BUFFER];
    let mut handle: Option<VolumeHandle> = None;
    let mut cursor: Option<Cursor> = load_cursor(&cursor_file);
    let mut last_flush = Instant::now();
    let mut last_health = Instant::now();
    let mut last_cursor_save = Instant::now();
    // An analysed batch the spool has not accepted yet, with the cursor it
    // would advance to once it does.
    let mut pending: Option<(Vec<ObservationRecord>, Cursor)> = None;
    let mut counters = Counters::default();
    let mut last_record_monotonic = shared.anchor().monotonic_ns;
    set_status(&shared, &volume, |s| s.state = FeedState::Starting);

    while !shared.is_shutting_down() && shared.lane_enabled(|c| c.lanes.usn) {
        // (Re)open the volume and establish a position.
        if handle.is_none() {
            match VolumeHandle::open_device(&volume.device, true) {
                Ok(opened) => match query_journal(&opened) {
                    Ok(info) => {
                        let resumed = match cursor {
                            Some(c) if c.journal_id == info.journal_id => {
                                if c.next_usn < info.first_usn {
                                    tracing::warn!(root = %volume.root(), "journal overflowed while the collector was away");
                                    shared.add_gap(
                                        GapCause::FeedOverflow,
                                        last_record_monotonic,
                                        None,
                                    );
                                    counters.overflows += 1;
                                    Cursor {
                                        journal_id: info.journal_id,
                                        next_usn: info.first_usn,
                                    }
                                } else {
                                    c
                                }
                            }
                            Some(_) => {
                                tracing::warn!(root = %volume.root(), "journal was recreated");
                                shared.add_gap(
                                    GapCause::JournalRecreated,
                                    last_record_monotonic,
                                    None,
                                );
                                counters.recreations += 1;
                                Cursor {
                                    journal_id: info.journal_id,
                                    next_usn: info.first_usn,
                                }
                            }
                            None => Cursor {
                                journal_id: info.journal_id,
                                next_usn: info.next_usn,
                            },
                        };
                        cursor = Some(resumed);
                        save_cursor(&cursor_file, &resumed);
                        handle = Some(opened);
                        counters.unavailable = false;
                        set_status(&shared, &volume, |s| {
                            s.state = FeedState::Live;
                            s.detail = None;
                        });
                    }
                    Err(error) => {
                        feed_unavailable(&shared, &volume, &error, &mut counters);
                        if wait_or_stop(&shared, &cancel, 30) {
                            break;
                        }
                        continue;
                    }
                },
                Err(error) => {
                    feed_unavailable(&shared, &volume, &error, &mut counters);
                    if wait_or_stop(&shared, &cancel, 30) {
                        break;
                    }
                    continue;
                }
            }
        }
        let (Some(vol), Some(position)) = (handle.as_ref(), cursor) else {
            continue;
        };

        // A batch that would not persist is kept, not re-derived. Re-reading
        // the range would put the same records through the analyzer a second
        // time and count the same filesystem activity twice in every rate,
        // reason, operation, edit, fan-out, tombstone, and coalescing figure.
        if let Some((batch, advanced)) = pending.take() {
            if shared.append_all_retrying(&batch) {
                cursor = Some(advanced);
                save_cursor(&cursor_file, &advanced);
                last_cursor_save = Instant::now();
                set_status(&shared, &volume, |s| {
                    s.state = FeedState::Live;
                    s.detail = None;
                    s.last_batch = Some(Instant::now());
                });
            } else {
                pending = Some((batch, advanced));
                if wait_or_stop(&shared, &cancel, 15) {
                    break;
                }
            }
        }

        // Wake at least once per summary interval: a volume with no activity
        // still owes rate summaries and feed-health records.
        let idle_wake = shared.config.lock().unwrap().intervals.rate_s.max(10) as u64;
        let outcome = if pending.is_some() {
            // Hold here until the spool takes what is already analysed.
            Ok(None)
        } else {
            read_journal_wait_timeout(
                vol,
                position.journal_id,
                position.next_usn,
                &mut buffer,
                &cancel,
                Duration::from_secs(idle_wake),
            )
        };
        match outcome {
            // Nothing arrived in time; fall through to the periodic flush.
            Ok(None) => {}
            Ok(Some(ReadOutcome::Records { records, next_usn })) => {
                let now = shared.anchor();
                // Cleared when a batch cannot be persisted, which pins the
                // cursor until the spool recovers.
                let mut durable = true;
                let mut changes = Vec::new();
                if !records.is_empty() {
                    last_record_monotonic = now.monotonic_ns;
                    counters.batches += 1;
                    counters.records += records.len() as u64;
                    // Read stable settings and clone the sender before the
                    // key is held. The health thread can take `config` and
                    // then `key`; taking them the other way round here would
                    // wedge both threads, and with them every stop. Lane
                    // enablement itself is mirrored atomically so a runtime
                    // change still takes effect within this batch.
                    let sample_percent = shared.config.lock().unwrap().lanes.content.sample_percent;
                    let nominations = shared.content_tx.lock().unwrap().clone();
                    // Do not enumerate directories when this batch cannot
                    // produce keyed detail. This is only an availability
                    // snapshot: the key is acquired again for analysis so no
                    // lock is held across filesystem I/O.
                    let key_available = shared.key.lock().unwrap().is_some();
                    if key_available {
                        let want_facts = records.len() <= FACT_LOOKUP_BATCH_LIMIT;
                        // Resolved before the study key is taken. Reading facts
                        // from the parent directory is a far longer operation
                        // than the per-file attribute read it replaced, and
                        // holding the key across a directory walk would stall
                        // every thread that needs it - a status request among
                        // them. One `Lookup` for the batch, so a churning
                        // directory is walked once however many of its children
                        // changed.
                        let facts: Vec<ObjectFacts> = {
                            let should_stop = || shared.is_shutting_down() || cancel.is_cancelled();
                            let mut lookup = crate::object_facts::Lookup::with_stop(
                                &mut size_cache,
                                &mut depth_cache,
                                &should_stop,
                            );
                            records
                                .iter()
                                .map(|record| {
                                    let view = view_of(record);
                                    if want_facts && ChangeAnalyzer::wants_facts(&view) {
                                        object_facts(vol, &view, &mut lookup)
                                    } else {
                                        ObjectFacts::default()
                                    }
                                })
                                .collect()
                        };
                        let key = shared.key.lock().unwrap();
                        if let Some(key) = key.as_ref() {
                            for (record, facts) in records.iter().zip(facts) {
                                let sampling = shared.content_enabled().then_some(sample_percent);
                                let view = view_of(record);
                                if let Some(change) =
                                    analyzer.observe(key, &view, facts, now.clone())
                                {
                                    nominate(
                                        &volume,
                                        &view,
                                        &change,
                                        key,
                                        sampling,
                                        nominations.as_ref(),
                                        &mut counters,
                                    );
                                    changes.push(ObservationRecord::LogicalChange(change));
                                }
                            }
                        } else {
                            // The key became unavailable during fact lookup.
                            counters.dropped_without_key += records.len() as u64;
                        }
                    } else {
                        counters.dropped_without_key += records.len() as u64;
                    }
                    counters.logical_changes += changes.len() as u64;
                    if !changes.is_empty() {
                        durable = shared.append_all_retrying(&changes);
                    }
                }
                let advanced = Cursor {
                    journal_id: position.journal_id,
                    next_usn,
                };
                if !durable {
                    // Hold the cursor and the batch together. Advancing past
                    // records that were not durably spooled would skip them on
                    // restart, and an in-memory gap marker beside a persisted
                    // cursor is exactly the evidence a crash destroys. Keeping
                    // the analysed batch means the retry costs no re-analysis;
                    // a restart re-reads the range against a fresh analyzer,
                    // which is equally correct.
                    counters.spool_failures += 1;
                    tracing::error!(
                        root = %volume.root(),
                        "holding the journal cursor: the spool is not accepting writes"
                    );
                    pending = Some((changes, advanced));
                    set_status(&shared, &volume, |s| {
                        s.state = FeedState::Starting;
                        s.detail = Some("spool writes are failing".into());
                    });
                    if wait_or_stop(&shared, &cancel, 15) {
                        break;
                    }
                } else {
                    cursor = Some(advanced);
                    if last_cursor_save.elapsed() >= Duration::from_secs(1) {
                        save_cursor(&cursor_file, &advanced);
                        last_cursor_save = Instant::now();
                    }
                    let lag = query_journal(vol)
                        .ok()
                        .map(|live| live.next_usn.saturating_sub(next_usn).max(0) as u64)
                        .unwrap_or(0);
                    set_status(&shared, &volume, |s| {
                        s.state = FeedState::Live;
                        s.batches = counters.batches;
                        s.records = counters.records;
                        s.logical_changes = counters.logical_changes;
                        s.lag_bytes = lag;
                        s.last_batch = Some(Instant::now());
                        s.probe_dropped = counters.probe_dropped;
                    });
                }
            }
            Ok(Some(ReadOutcome::EntryDeleted)) => {
                tracing::warn!(root = %volume.root(), "journal overflowed");
                shared.add_gap(GapCause::FeedOverflow, last_record_monotonic, None);
                counters.overflows += 1;
                set_status(&shared, &volume, |s| {
                    s.state = FeedState::Overflowed;
                    s.overflows = counters.overflows;
                });
                handle = None;
            }
            Ok(Some(ReadOutcome::JournalChanged)) => {
                tracing::warn!(root = %volume.root(), "journal recreated");
                shared.add_gap(GapCause::JournalRecreated, last_record_monotonic, None);
                counters.recreations += 1;
                cursor = None;
                set_status(&shared, &volume, |s| {
                    s.state = FeedState::Recreated;
                    s.recreations = counters.recreations;
                });
                handle = None;
            }
            Err(UsnError::Cancelled) => break,
            Err(error) => {
                counters.read_errors += 1;
                feed_unavailable(&shared, &volume, &error, &mut counters);
                handle = None;
                if wait_or_stop(&shared, &cancel, 15) {
                    break;
                }
            }
        }

        let rate_s = shared.config.lock().unwrap().intervals.rate_s.max(10) as u64;
        if last_flush.elapsed() >= Duration::from_secs(rate_s) {
            last_flush = Instant::now();
            flush_interval(
                &shared,
                &mut analyzer,
                &mut counters,
                cursor,
                handle.as_ref(),
                &mut last_health,
            );
        }
    }
    if let Some(c) = cursor {
        save_cursor(&cursor_file, &c);
    }
    flush_interval(
        &shared,
        &mut analyzer,
        &mut counters,
        cursor,
        handle.as_ref(),
        &mut last_health,
    );
    set_status(&shared, &volume, |s| s.state = FeedState::Stopped);
    tracing::info!(root = %volume.root(), records = counters.records, "usn reader stopped");
}

/// Offer a closed-after-write file to the content probe when that lane is
/// on and the deterministic sample selects the object.
/// `sampling` is the content lane's sample rate and `sender` the probe's
/// queue, both read once per batch by the caller. This runs with the study
/// key held, so it takes no lock of its own - not the key it is given, and
/// not the queue it sends to.
fn nominate(
    volume: &VolumeFacts,
    view: &RecordView<'_>,
    change: &eidos_observe::LogicalChange,
    key: &eidos_observe::StudyKey,
    sampling: Option<u8>,
    sender: Option<&std::sync::mpsc::SyncSender<crate::content_probe::Candidate>>,
    counters: &mut Counters,
) {
    use eidos_observe::ChangeOperation;
    if view.is_directory
        || !matches!(
            change.operation,
            ChangeOperation::Create | ChangeOperation::Update
        )
    {
        return;
    }
    let (Some(percent), Some(sender)) = (sampling, sender) else {
        return;
    };
    if !crate::content_probe::selected(key, volume.guid_path.as_bytes(), view.frn, percent) {
        return;
    }
    {
        crate::content_probe::offer(
            sender,
            crate::content_probe::Candidate {
                device: volume.device.clone(),
                volume_id: volume.guid_path.as_bytes().to_vec(),
                frn: view.frn,
                extension: change.extension,
                queued: Instant::now(),
            },
            &mut counters.probe_dropped,
        );
    }
}

#[derive(Default)]
struct Counters {
    batches: u64,
    records: u64,
    logical_changes: u64,
    overflows: u64,
    recreations: u64,
    read_errors: u64,
    dropped_without_key: u64,
    spool_failures: u64,
    coalesced_total: u64,
    unavailable: bool,
    probe_dropped: u64,
}

fn flush_interval(
    shared: &Shared,
    analyzer: &mut ChangeAnalyzer,
    counters: &mut Counters,
    cursor: Option<Cursor>,
    handle: Option<&VolumeHandle>,
    last_health: &mut Instant,
) {
    let now = shared.anchor();
    let Some(output) = shared.with_key(|key| analyzer.flush(key, now.clone())) else {
        return;
    };
    counters.coalesced_total += output.coalesced;
    shared.drops.lock().unwrap().coalesced += output.coalesced;
    let mut records = vec![
        ObservationRecord::Rate(output.rate),
        ObservationRecord::Reasons(output.reasons),
    ];
    let health_s = shared
        .config
        .lock()
        .unwrap()
        .intervals
        .feed_health_s
        .max(30) as u64;
    if last_health.elapsed() >= Duration::from_secs(health_s) {
        *last_health = Instant::now();
        let live = handle.and_then(|h| query_journal(h).ok());
        let (lag, fill) = match (live, cursor) {
            (Some(live), Some(c)) => (
                bucket_size(live.next_usn.saturating_sub(c.next_usn).max(0) as u64),
                PercentBucket::from_ratio(
                    live.next_usn.saturating_sub(live.first_usn).max(0) as u64,
                    live.maximum_size.max(1),
                ),
            ),
            _ => (eidos_observe::SizeBucket::Unknown, PercentBucket::Zero),
        };
        let state = if handle.is_some() {
            FeedState::Live
        } else {
            FeedState::Offline
        };
        let volume_token = shared.with_key(|key| key.token("volume", analyzer.volume_id()));
        if let Some(volume) = volume_token {
            records.push(ObservationRecord::FeedHealth(FeedHealthRecord {
                at: now,
                volume,
                feed: FeedKind::Usn,
                state,
                cursor: cursor.map(|c| c.feed_cursor()),
                lag,
                fill,
                batches: counters.batches,
                records: counters.records,
                logical_changes: counters.logical_changes,
                coalesced: counters.coalesced_total,
                overflows: counters.overflows,
                recreations: counters.recreations,
                read_errors: counters.read_errors,
                backlog_ms: output.backlog_ms,
            }));
        }
    }
    if let Err(error) = shared.spool.lock().unwrap().append_all(&records) {
        tracing::error!(error = %error, "spool summary batch failed");
    }
}

fn feed_unavailable(
    shared: &Shared,
    volume: &VolumeFacts,
    error: &UsnError,
    counters: &mut Counters,
) {
    let state = match error {
        UsnError::AccessDenied => FeedState::AccessDenied,
        UsnError::NotActive | UsnError::DeleteInProgress => FeedState::NotActive,
        UsnError::Unsupported => FeedState::Unsupported,
        UsnError::Cancelled => FeedState::Stopped,
        UsnError::Io(_) => FeedState::Offline,
    };
    // One gap per unavailable episode, logged once; retries are silent.
    if !counters.unavailable {
        counters.unavailable = true;
        tracing::warn!(root = %volume.root(), error = %error, "journal unavailable");
        shared.add_gap(
            GapCause::FeedUnavailable,
            shared.anchor().monotonic_ns,
            None,
        );
    }
    set_status(shared, volume, |s| {
        s.state = state;
        s.detail = Some(error.to_string());
    });
}

/// Sleep in one-second steps so shutdown and lane switches stay responsive.
/// Returns true when the reader should stop.
fn wait_or_stop(shared: &Shared, cancel: &JournalCancellation, seconds: u64) -> bool {
    for _ in 0..seconds {
        if shared.is_shutting_down()
            || !shared.lane_enabled(|c| c.lanes.usn)
            || cancel.is_cancelled()
        {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

/// The analyzer's borrowed view of one journal record.
fn view_of(record: &eidos_scanner::usn::UsnRecord) -> RecordView<'_> {
    RecordView {
        usn: record.usn,
        frn: record.frn,
        parent_frn: record.parent_frn,
        reason: record.reason,
        is_directory: record.is_directory(),
        name: &record.name,
        timestamp_ns: record.timestamp.0,
    }
}

/// Size and depth for one changed object.
///
/// Reads them from the object's *parent directory*, never from the object.
/// Opening the file here is what used to make the collector break other
/// software on the host: a handle held on a file - however permissive its
/// sharing flags - turns a concurrent delete into a delete-pending and makes
/// the next create on that path fail with ACCESS_DENIED. Facts are wanted on
/// close records, which is precisely when a writer is most likely to be about
/// to delete and recreate. See `object_facts` for the whole argument.
fn object_facts(
    vol: &VolumeHandle,
    record: &RecordView<'_>,
    lookup: &mut crate::object_facts::Lookup<'_>,
) -> ObjectFacts {
    let (size, depth) = lookup.facts(vol, record.frn, record.parent_frn);
    ObjectFacts { size, depth }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_opaque_and_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v.cursor");
        let cursor = Cursor {
            journal_id: 0x1122,
            next_usn: 0x3344,
        };
        save_cursor(&path, &cursor);
        assert_eq!(load_cursor(&path), Some(cursor));
        let feed = cursor.feed_cursor();
        assert_eq!(feed.feed, FeedKind::Usn);
        assert_eq!(feed.opaque.len(), 32);
        assert!(!feed.opaque.contains("1122"));
    }
}
