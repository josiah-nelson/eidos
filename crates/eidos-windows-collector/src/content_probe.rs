//! L2: content economics on a sample of files closed after a write. The USN
//! lane nominates candidates; this thread reads them under a byte budget
//! and records chunk shape, keyed fingerprints, compression ratio, and
//! chunk reuse against the previous observation of the same object. It
//! never opens a placeholder, offline, or reparse-point file.

use crate::cdc::{looks_textual, CdcParams, Chunker};
use crate::daemon::Shared;
use eidos_domain::FileAttributes;
use eidos_observe::{
    bucket_size, ChunkerKind, ContentObservation, ContentOutcome, ExtensionBucket, Histogram,
    ObservationRecord, PercentBucket,
};
use eidos_scanner::usn::{snapshot_by_id, VolumeHandle};
use lru::LruCache;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    ExtendedFileIdType, OpenFileById, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_128, FILE_ID_DESCRIPTOR,
    FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub const QUEUE_DEPTH: usize = 1024;
const SETTLE: Duration = Duration::from_secs(3);
const MAX_REMEMBERED_CHUNKS: usize = 16_384;
const ERROR_SHARING_VIOLATION: u32 = 32;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_INVALID_PARAMETER: u32 = 87;

#[derive(Debug)]
pub struct Candidate {
    pub device: String,
    pub volume_id: Vec<u8>,
    pub frn: u128,
    pub extension: ExtensionBucket,
    pub queued: Instant,
}

/// Deterministic sampling on the object identity so the same objects are
/// followed over time (needed for chunk-reuse measurement).
pub fn selected(shared: &Shared, volume_id: &[u8], frn: u128, sample_percent: u8) -> bool {
    if sample_percent >= 100 {
        return true;
    }
    let mut identity = volume_id.to_vec();
    identity.extend_from_slice(&frn.to_le_bytes());
    shared
        .with_key(|key| {
            let mut hasher = key.hasher("sample");
            hasher.update(&identity);
            (hasher.finish_digest()[0] as u32 * 100 / 256) < sample_percent as u32
        })
        .unwrap_or(false)
}

pub fn offer(sender: &SyncSender<Candidate>, candidate: Candidate, dropped: &mut u64) {
    match sender.try_send(candidate) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => *dropped += 1,
    }
}

pub fn start(shared: Arc<Shared>, receiver: Receiver<Candidate>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("content-probe".into())
        .spawn(move || run(shared, receiver))
        .expect("spawn content probe")
}

struct Budget {
    hour_started: Instant,
    spent: u64,
}

fn run(shared: Arc<Shared>, receiver: Receiver<Candidate>) {
    let mut handles: HashMap<String, VolumeHandle> = HashMap::new();
    let mut previous: LruCache<(Vec<u8>, u128), Vec<[u8; 32]>> =
        LruCache::new(NonZeroUsize::new(4_096).unwrap());
    let mut budget = Budget {
        hour_started: Instant::now(),
        spent: 0,
    };
    let mut records = Vec::new();
    let mut last_flush = Instant::now();
    while !shared.is_shutting_down() {
        let candidate = match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(candidate) => Some(candidate),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(candidate) = candidate {
            let (enabled, max_bytes, hourly) = {
                let config = shared.config.lock().unwrap();
                (
                    config.lanes.content.enabled,
                    config.lanes.content.max_bytes,
                    config.lanes.content.hourly_budget_bytes,
                )
            };
            if enabled {
                if budget.hour_started.elapsed() >= Duration::from_secs(3600) {
                    budget = Budget {
                        hour_started: Instant::now(),
                        spent: 0,
                    };
                }
                let settle = SETTLE.saturating_sub(candidate.queued.elapsed());
                if !settle.is_zero() {
                    std::thread::sleep(settle);
                }
                if let Some(record) = measure(
                    &shared,
                    &mut handles,
                    &mut previous,
                    &mut budget,
                    max_bytes,
                    hourly,
                    &candidate,
                ) {
                    records.push(ObservationRecord::Content(record));
                }
            }
        }
        if !records.is_empty()
            && (records.len() >= 64 || last_flush.elapsed() >= Duration::from_secs(10))
        {
            if let Err(error) = shared.spool.lock().unwrap().append_all(&records) {
                tracing::error!(error = %error, "spool content batch failed");
            }
            records.clear();
            last_flush = Instant::now();
        }
    }
    if !records.is_empty() {
        let _ = shared.spool.lock().unwrap().append_all(&records);
    }
}

fn measure(
    shared: &Shared,
    handles: &mut HashMap<String, VolumeHandle>,
    previous: &mut LruCache<(Vec<u8>, u128), Vec<[u8; 32]>>,
    budget: &mut Budget,
    max_bytes: u64,
    hourly: u64,
    candidate: &Candidate,
) -> Option<ContentObservation> {
    let (volume, object) = shared.with_key(|key| {
        let mut identity = candidate.volume_id.clone();
        identity.extend_from_slice(&candidate.frn.to_le_bytes());
        (
            key.token("volume", &candidate.volume_id),
            key.token("object", &identity),
        )
    })?;
    let base = |outcome: ContentOutcome, size: u64| ContentObservation {
        at: shared.anchor(),
        volume: volume.clone(),
        object: object.clone(),
        size: bucket_size(size),
        extension: candidate.extension,
        outcome,
        fingerprint: None,
        chunker: ChunkerKind::None,
        chunks: 0,
        chunk_size: Histogram::new(),
        reused_chunks: 0,
        reuse_runs: Histogram::new(),
        compressed: PercentBucket::Zero,
        read_ms: 0,
        text_like: None,
    };
    if !handles.contains_key(&candidate.device) {
        match VolumeHandle::open_device(&candidate.device, false) {
            Ok(handle) => {
                handles.insert(candidate.device.clone(), handle);
            }
            Err(_) => return Some(base(ContentOutcome::Error, 0)),
        }
    }
    let volume_handle = handles.get(&candidate.device)?;
    let snapshot = match snapshot_by_id(volume_handle, candidate.frn) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Some(base(ContentOutcome::Vanished, 0)),
        Err(_) => return Some(base(ContentOutcome::Error, 0)),
    };
    let attributes = snapshot.attributes;
    if attributes.is_reparse() || snapshot.reparse_tag != 0 {
        return Some(base(ContentOutcome::SkippedReparse, snapshot.size));
    }
    if attributes.has(FileAttributes::OFFLINE)
        || attributes.has(FileAttributes::RECALL_ON_DATA_ACCESS)
    {
        return Some(base(ContentOutcome::SkippedPlaceholder, snapshot.size));
    }
    if attributes.is_directory() {
        return None;
    }
    if snapshot.size > max_bytes {
        return Some(base(ContentOutcome::SkippedTooLarge, snapshot.size));
    }
    if budget.spent.saturating_add(snapshot.size) > hourly {
        return Some(base(ContentOutcome::SkippedTooLarge, snapshot.size));
    }
    let mut file = match open_for_read(volume_handle, candidate.frn) {
        Ok(file) => file,
        Err(ERROR_SHARING_VIOLATION) => {
            return Some(base(ContentOutcome::SkippedSharing, snapshot.size))
        }
        Err(ERROR_ACCESS_DENIED) => {
            return Some(base(ContentOutcome::SkippedAccessDenied, snapshot.size))
        }
        Err(ERROR_FILE_NOT_FOUND) | Err(ERROR_INVALID_PARAMETER) => {
            return Some(base(ContentOutcome::Vanished, snapshot.size))
        }
        Err(_) => return Some(base(ContentOutcome::Error, snapshot.size)),
    };

    let started = Instant::now();
    let params = CdcParams::DEFAULT;
    let mut chunker = Chunker::new(params);
    let mut chunk_size = Histogram::new();
    let mut chunk_digests: Vec<[u8; 32]> = Vec::new();
    let mut compressed = CountingSink(0);
    let mut encoder = match zstd::stream::write::Encoder::new(&mut compressed, 3) {
        Ok(encoder) => encoder,
        Err(_) => return Some(base(ContentOutcome::Error, snapshot.size)),
    };
    let (mut fingerprint, mut chunk_hasher) =
        shared.with_key(|key| (key.hasher("content"), key.hasher("chunk")))?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    let mut text_sample: Option<bool> = None;
    let mut chunk_bytes: Vec<u8> = Vec::with_capacity(params.max);
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => {
                // Bytes already read cost the same whether or not the file
                // finished, so charge them before giving up.
                budget.spent = budget.spent.saturating_add(total);
                return Some(base(ContentOutcome::Error, snapshot.size));
            }
        };
        let bytes = &buffer[..read];
        if text_sample.is_none() {
            text_sample = looks_textual(&bytes[..read.min(8192)]);
        }
        total += read as u64;
        // A file can grow after its size snapshot, or between the precheck and
        // this read, so the running total is what the budget must be held to.
        if total > max_bytes || budget.spent.saturating_add(total) > hourly {
            budget.spent = budget.spent.saturating_add(total);
            return Some(base(ContentOutcome::SkippedTooLarge, total));
        }
        fingerprint.update(bytes);
        let _ = encoder.write_all(bytes);
        // Chunk digests are computed over each closed chunk's bytes.
        let mut consumed_in_piece = 0usize;
        chunker.update(bytes, |length| {
            let take = length - chunk_bytes.len();
            chunk_bytes.extend_from_slice(&bytes[consumed_in_piece..consumed_in_piece + take]);
            consumed_in_piece += take;
            chunk_hasher.update(&chunk_bytes);
            chunk_digests.push(chunk_hasher.finish_digest());
            chunk_hasher = shared
                .with_key(|key| key.hasher("chunk"))
                .expect("key present during measurement");
            chunk_size.observe(length as u64);
            chunk_bytes.clear();
        });
        chunk_bytes.extend_from_slice(&bytes[consumed_in_piece..]);
    }
    if let Some(last) = chunker.finish() {
        chunk_hasher.update(&chunk_bytes);
        chunk_digests.push(chunk_hasher.finish_digest());
        chunk_size.observe(last as u64);
    }
    let _ = encoder.finish();
    budget.spent = budget.spent.saturating_add(total);

    let key = (candidate.volume_id.clone(), candidate.frn);
    let (reused, runs) = match previous.get(&key) {
        Some(old) => reuse(old, &chunk_digests),
        None => (0, Histogram::new()),
    };
    // Count every chunk the file produced; `remembered` is only the bounded
    // history kept for the next reuse comparison.
    let chunks = chunk_digests.len() as u32;
    let mut remembered = chunk_digests;
    remembered.truncate(MAX_REMEMBERED_CHUNKS);
    previous.put(key, remembered);

    Some(ContentObservation {
        at: shared.anchor(),
        volume,
        object,
        size: bucket_size(total),
        extension: candidate.extension,
        outcome: ContentOutcome::Measured,
        fingerprint: Some(fingerprint.finish()),
        chunker: ChunkerKind::FastCdc {
            min: params.min as u32,
            average: params.average as u32,
            max: params.max as u32,
        },
        chunks,
        chunk_size,
        reused_chunks: reused,
        reuse_runs: runs,
        compressed: PercentBucket::from_ratio(compressed.0, total.max(1)),
        read_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
        text_like: text_sample,
    })
}

/// Chunks of `new` whose digest appeared in `old`, and the lengths of
/// contiguous reused runs.
pub fn reuse(old: &[[u8; 32]], new: &[[u8; 32]]) -> (u32, Histogram) {
    let known: std::collections::HashSet<&[u8; 32]> = old.iter().collect();
    let mut reused = 0u32;
    let mut runs = Histogram::new();
    let mut run = 0u64;
    for digest in new {
        if known.contains(digest) {
            reused += 1;
            run += 1;
        } else if run > 0 {
            runs.observe(run);
            run = 0;
        }
    }
    if run > 0 {
        runs.observe(run);
    }
    (reused, runs)
}

struct CountingSink(u64);

impl Write for CountingSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Open a file by reference number for sequential reading without any
/// reparse-point flag: the caller has already verified the object is not a
/// reparse point or placeholder from a non-hydrating attribute snapshot.
fn open_for_read(vol: &VolumeHandle, frn: u128) -> Result<std::fs::File, u32> {
    let mut descriptor = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: ExtendedFileIdType,
        ..Default::default()
    };
    descriptor.Anonymous.ExtendedFileId = FILE_ID_128 {
        Identifier: frn.to_le_bytes(),
    };
    // SAFETY: descriptor fully initialised; volume handle valid.
    let handle = unsafe {
        OpenFileById(
            vol.raw(),
            &descriptor,
            FILE_READ_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_SEQUENTIAL_SCAN,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe.
        return Err(unsafe { GetLastError() });
    }
    // SAFETY: a fresh file handle owned by the returned File.
    Ok(unsafe { std::fs::File::from_raw_handle(handle as _) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_counts_matches_and_runs() {
        let old = [[1u8; 32], [2; 32], [3; 32], [4; 32]];
        let new = [[1u8; 32], [2; 32], [9; 32], [4; 32], [8; 32]];
        let (reused, runs) = reuse(&old, &new);
        assert_eq!(reused, 3);
        assert_eq!(runs.total, 2);
        assert_eq!(runs.max, 2);
    }
}
