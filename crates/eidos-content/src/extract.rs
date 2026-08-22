//! Extraction orchestrator: open → sniff → decode/chunk → hash → sink.

use crate::chunk::{Chunk, Chunker, ChunkerConfig};
use crate::sniff::{sniff, Encoding, Sniff};
use eidos_domain::{ContentId, ContentState, Coverage, FailureClass};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Bytes examined for encoding detection.
    pub sniff_bytes: usize,
    /// Read buffer size.
    pub read_bytes: usize,
    /// Beyond this many bytes only a prefix is indexed (`coverage = prefix`).
    pub max_full_bytes: u64,
    pub chunker: ChunkerConfig,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sniff_bytes: 8 * 1024,
            read_bytes: 1024 * 1024,
            max_full_bytes: 4 * 1024 * 1024 * 1024,
            chunker: ChunkerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub state: ContentState,
    pub coverage: Coverage,
    pub encoding: Option<Encoding>,
    pub total_bytes: u64,
    pub indexed_bytes: u64,
    pub chunk_count: u32,
    pub line_count: u64,
    pub chars: u64,
    /// BLAKE3 of the bytes read (full file only when `hash_complete`).
    pub content_id: Option<ContentId>,
    pub hash_complete: bool,
    pub failure: Option<(FailureClass, String)>,
    /// Human-readable reason for `unsupported`/`excluded` outcomes.
    pub reason: Option<String>,
    pub elapsed_ms: f64,
}

fn open_shared(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Never block writers or deleters; hint sequential access.
        o.share_mode(0x1 | 0x2 | 0x4);
        o.custom_flags(0x0800_0000);
    }
    o.open(path)
}

fn classify_io(e: &std::io::Error) -> FailureClass {
    match e.raw_os_error() {
        Some(32) | Some(33) | Some(21) | Some(53) | Some(64) | Some(59) | Some(1450)
        | Some(170) => FailureClass::Transient,
        _ => match e.kind() {
            std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock => FailureClass::Transient,
            _ => FailureClass::Deterministic,
        },
    }
}

/// Extract literal text from `path`, delivering chunks to `sink` as they are
/// produced. Memory use is bounded by `limits.read_bytes` plus one chunk.
pub fn extract(
    path: &Path,
    limits: &Limits,
    sink: &mut dyn FnMut(Chunk) -> Result<(), String>,
) -> Outcome {
    let started = Instant::now();
    let mut out = Outcome {
        state: ContentState::Failed,
        coverage: Coverage::None,
        encoding: None,
        total_bytes: 0,
        indexed_bytes: 0,
        chunk_count: 0,
        line_count: 0,
        chars: 0,
        content_id: None,
        hash_complete: false,
        failure: None,
        reason: None,
        elapsed_ms: 0.0,
    };
    let mut file = match open_shared(path) {
        Ok(f) => f,
        Err(e) => {
            out.failure = Some((classify_io(&e), format!("open: {e}")));
            out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            return out;
        }
    };
    out.total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; limits.read_bytes.max(4096)];

    // Sniff from the first read.
    let mut head_len = 0usize;
    let sniff_len = limits.sniff_bytes.min(buf.len());
    while head_len < sniff_len {
        match file.read(&mut buf[head_len..sniff_len]) {
            Ok(0) => break,
            Ok(n) => head_len += n,
            Err(e) => {
                out.failure = Some((classify_io(&e), format!("read: {e}")));
                out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                return out;
            }
        }
    }
    let (encoding, bom_len) = match sniff(&buf[..head_len]) {
        Sniff::Text { encoding, bom_len } => (encoding, bom_len),
        Sniff::Binary { reason } => {
            out.state = ContentState::Unsupported;
            out.reason = Some(format!("binary: {reason}"));
            out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            return out;
        }
    };
    out.encoding = Some(encoding);
    hasher.update(&buf[..head_len]);
    let mut chunker = Chunker::new(encoding, bom_len as u64, limits.chunker);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut consumed: u64 = head_len as u64;
    let mut truncated = false;

    chunker.push(&buf[bom_len.min(head_len)..head_len], &mut chunks);
    if let Err(e) = deliver(&mut chunks, sink, &mut out) {
        out.failure = Some((FailureClass::Deterministic, e));
        out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        return out;
    }

    loop {
        if consumed >= limits.max_full_bytes {
            truncated = true;
            break;
        }
        let want = buf
            .len()
            .min((limits.max_full_bytes - consumed).min(usize::MAX as u64) as usize);
        let n = match file.read(&mut buf[..want.max(1)]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                out.failure = Some((classify_io(&e), format!("read: {e}")));
                out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                return out;
            }
        };
        hasher.update(&buf[..n]);
        consumed += n as u64;
        chunker.push(&buf[..n], &mut chunks);
        if let Err(e) = deliver(&mut chunks, sink, &mut out) {
            out.failure = Some((FailureClass::Deterministic, e));
            out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            return out;
        }
    }
    chunker.finish(&mut chunks);
    if let Err(e) = deliver(&mut chunks, sink, &mut out) {
        out.failure = Some((FailureClass::Deterministic, e));
        out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        return out;
    }
    out.indexed_bytes = consumed;
    out.line_count = chunker.lines_total;
    out.chars = chunker.chars_total;
    out.content_id = Some(ContentId(*hasher.finalize().as_bytes()));
    if truncated && consumed < out.total_bytes {
        out.coverage = Coverage::Prefix;
        out.state = ContentState::Partial;
        out.hash_complete = false;
        out.reason = Some(format!(
            "indexed the first {} of {} bytes (max_full_bytes)",
            consumed, out.total_bytes
        ));
    } else {
        out.coverage = Coverage::Full;
        out.state = ContentState::Indexed;
        out.hash_complete = true;
        out.total_bytes = out.total_bytes.max(consumed);
    }
    out.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    out
}

fn deliver(
    chunks: &mut Vec<Chunk>,
    sink: &mut dyn FnMut(Chunk) -> Result<(), String>,
    out: &mut Outcome,
) -> Result<(), String> {
    for c in chunks.drain(..) {
        out.chunk_count += 1;
        sink(c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(path: &Path, limits: &Limits) -> (Outcome, Vec<Chunk>) {
        let mut chunks = Vec::new();
        let o = extract(path, limits, &mut |c| {
            chunks.push(c);
            Ok(())
        });
        (o, chunks)
    }

    #[test]
    fn utf8_file_full_coverage_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        let body = "line one\nline two with QzEndpoint\nthird\n".repeat(500);
        std::fs::write(&p, &body).unwrap();
        let (o, chunks) = collect(&p, &Limits::default());
        assert_eq!(o.state, ContentState::Indexed);
        assert_eq!(o.coverage, Coverage::Full);
        assert_eq!(o.encoding, Some(Encoding::Utf8));
        assert_eq!(o.total_bytes, body.len() as u64);
        assert_eq!(o.indexed_bytes, body.len() as u64);
        assert_eq!(o.line_count, 1500);
        assert!(o.hash_complete);
        assert_eq!(
            o.content_id.unwrap().0,
            *blake3::hash(body.as_bytes()).as_bytes()
        );
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, body);
        assert_eq!(chunks.len() as u32, o.chunk_count);
        assert!(chunks.len() > 1);
    }

    #[test]
    fn binary_is_unsupported_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        std::fs::write(&p, [0u8, 1, 2, 3, 0xFF, 0, 0, 9, 8, 7]).unwrap();
        let (o, chunks) = collect(&p, &Limits::default());
        assert_eq!(o.state, ContentState::Unsupported);
        assert!(o.reason.unwrap().contains("binary"));
        assert!(chunks.is_empty());
        assert!(o.failure.is_none());
    }

    #[test]
    fn prefix_coverage_when_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.log");
        let body = "0123456789\n".repeat(10_000); // 110 KB
        std::fs::write(&p, &body).unwrap();
        let limits = Limits {
            read_bytes: 4096,
            max_full_bytes: 50_000,
            ..Default::default()
        };
        let (o, chunks) = collect(&p, &limits);
        assert_eq!(o.state, ContentState::Partial);
        assert_eq!(o.coverage, Coverage::Prefix);
        assert!(o.indexed_bytes >= 50_000 && o.indexed_bytes < body.len() as u64);
        assert!(!o.hash_complete);
        assert_eq!(chunks.last().unwrap().byte_end, o.indexed_bytes);
    }

    #[test]
    fn streaming_large_file_contiguous_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("large.txt");
        {
            use std::io::Write;
            let mut f = std::io::BufWriter::new(std::fs::File::create(&p).unwrap());
            for i in 0..400_000u32 {
                writeln!(
                    f,
                    "2026-08-22 00:00:{:02} INFO worker-{} processed request id={i} status=ok",
                    i % 60,
                    i % 7
                )
                .unwrap();
            }
        }
        let size = std::fs::metadata(&p).unwrap().len();
        assert!(size > 25_000_000, "{size}");
        let mut max_chunk = 0usize;
        let mut last_end = 0u64;
        let mut n = 0u32;
        let o = extract(&p, &Limits::default(), &mut |c| {
            assert_eq!(c.byte_start, last_end, "chunks are contiguous");
            last_end = c.byte_end;
            max_chunk = max_chunk.max(c.text.len());
            n += 1;
            Ok(())
        });
        assert_eq!(o.state, ContentState::Indexed);
        assert_eq!(last_end, size);
        assert_eq!(o.line_count, 400_000);
        assert_eq!(n, o.chunk_count);
        assert!(max_chunk < 64 * 1024, "chunk text bounded: {max_chunk}");
        assert!(o.elapsed_ms < 30_000.0);
    }

    #[test]
    fn missing_file_is_transient_or_deterministic_failure() {
        let (o, _) = collect(Path::new("Z:\\definitely\\missing.txt"), &Limits::default());
        assert_eq!(o.state, ContentState::Failed);
        assert!(o.failure.is_some());
    }

    #[test]
    fn sink_error_fails_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello\n").unwrap();
        let o = extract(&p, &Limits::default(), &mut |_| Err("boom".into()));
        assert_eq!(o.state, ContentState::Failed);
        assert_eq!(o.failure.unwrap().0, FailureClass::Deterministic);
    }
}
