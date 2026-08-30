//! Content-transfer bakeoff (sprint section 6): measures candidate payload
//! strategies on deterministic synthetic fixtures (and, optionally, a
//! read-only corpus) so the release picks one bounded strategy - or ships
//! metadata-only replication - on evidence rather than on a hunch.
//!
//! Two independent decisions are measured:
//!
//! - **payload reuse**: whole compressed content, fixed-size chunks, and
//!   content-defined chunks (FastCDC at two target sizes);
//! - **protocol batch framing**: how many frames a version's payload takes
//!   under the row/byte bounds the transport enforces.
//!
//! Nothing here is a wire format. The harness stages a "central" that
//! already holds version A of each file and measures what shipping version
//! B costs under each strategy, including an interrupted transfer resumed
//! half way. Chunk identity is the BLAKE3 of the chunk bytes; a chunk the
//! central already holds is "reused" and costs a 40-byte manifest entry.

use eidos_content::cdc::{CdcParams, Chunker};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Bytes per protocol frame assumed for the framing measurement.
pub const FRAME_BYTES: usize = crate::wire::DEFAULT_BATCH_BYTES;
/// Rows per protocol frame assumed for the framing measurement.
pub const FRAME_ROWS: u64 = crate::wire::DEFAULT_BATCH_ROWS as u64;
/// Manifest entry per chunk: 32-byte hash + 8-byte length.
const MANIFEST_ENTRY: u64 = 40;
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The whole extracted content, compressed, shipped again on every
    /// change.
    WholeCompressed,
    /// Fixed 64 KiB chunks, each compressed and reused by hash.
    Fixed64K,
    /// FastCDC, 16 KiB average (4-64 KiB), each chunk compressed.
    Cdc16K,
    /// FastCDC, 64 KiB average (16-256 KiB), each chunk compressed.
    Cdc64K,
}

impl Strategy {
    pub const ALL: [Strategy; 4] = [
        Strategy::WholeCompressed,
        Strategy::Fixed64K,
        Strategy::Cdc16K,
        Strategy::Cdc64K,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Strategy::WholeCompressed => "whole_compressed",
            Strategy::Fixed64K => "fixed_64k",
            Strategy::Cdc16K => "cdc_16k",
            Strategy::Cdc64K => "cdc_64k",
        }
    }
}

/// One measured transfer: version B shipped to a central holding A.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub scenario: String,
    pub strategy: Strategy,
    /// The content extractor rejected this input before transfer.
    pub rejected: bool,
    pub source_bytes: u64,
    pub compressed_bytes: u64,
    /// Bytes put on the wire, manifest included.
    pub transferred_bytes: u64,
    pub reused_bytes: u64,
    pub novel_bytes: u64,
    pub chunks: u64,
    pub frames: u64,
    pub hash_ms: f64,
    pub chunk_ms: f64,
    pub compress_ms: f64,
    pub stage_ms: f64,
    pub apply_ms: f64,
    /// Largest buffer held at once by the shipper.
    pub peak_working_set_bytes: u64,
    /// Durable staging the central holds before the version is complete.
    pub staging_bytes: u64,
    /// Bytes shipped again after an interruption at half the transfer.
    pub recovery_bytes: u64,
    pub recovery_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: &'static str,
    pub frame_bytes: u64,
    pub frame_rows: u64,
    pub generated_at: String,
    pub measurements: Vec<Measurement>,
    pub summary: Vec<StrategySummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategySummary {
    pub strategy: Strategy,
    pub scenarios: u64,
    pub rejected_scenarios: u64,
    pub source_bytes: u64,
    pub transferred_bytes: u64,
    /// `transferred / source` over every scenario.
    pub transfer_ratio: f64,
    pub cpu_ms: f64,
    pub frames: u64,
    pub recovery_bytes: u64,
}

struct Version {
    name: String,
    a: Vec<u8>,
    b: Vec<u8>,
}

fn pseudo_random(len: usize, seed: &str) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed.as_bytes());
    let mut out = vec![0u8; len];
    hasher.finalize_xof().fill(&mut out);
    out
}

/// Deterministic text: numbered lines of words drawn from a small
/// vocabulary, so it compresses like prose and chunks like a log.
fn text(len: usize, seed: &str) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "catalog",
        "source",
        "epoch",
        "sequence",
        "watermark",
        "batch",
        "replica",
        "central",
        "node",
        "durable",
        "commit",
        "acknowledge",
        "compaction",
        "tombstone",
        "merkle",
        "repair",
        "session",
        "credit",
        "frame",
        "offer",
        "resume",
        "fence",
        "history",
        "chain",
    ];
    let noise = pseudo_random(len / 4 + 16, seed);
    let mut out = Vec::with_capacity(len + 64);
    let mut line = 0u64;
    let mut i = 0;
    while out.len() < len {
        out.extend_from_slice(format!("{line:08} ").as_bytes());
        let words = 6 + (noise[i % noise.len()] % 8) as usize;
        for w in 0..words {
            let pick = noise[(i + w) % noise.len()] as usize % WORDS.len();
            out.extend_from_slice(WORDS[pick].as_bytes());
            out.push(b' ');
        }
        out.push(b'\n');
        line += 1;
        i += words;
    }
    out.truncate(len);
    out
}

fn versions() -> Vec<Version> {
    let base = text(8 << 20, "base");
    let mut v = Vec::new();
    v.push(Version {
        name: "identical_on_two_hosts".into(),
        a: base.clone(),
        b: base.clone(),
    });
    let mut appended = base.clone();
    appended.extend_from_slice(&text(256 << 10, "append"));
    v.push(Version {
        name: "append".into(),
        a: base.clone(),
        b: appended,
    });
    let mut prepended = text(256 << 10, "prepend");
    prepended.extend_from_slice(&base);
    v.push(Version {
        name: "prepend".into(),
        a: base.clone(),
        b: prepended,
    });
    let mut inserted = base[..3 << 20].to_vec();
    inserted.extend_from_slice(&text(4 << 10, "insert"));
    inserted.extend_from_slice(&base[3 << 20..]);
    v.push(Version {
        name: "localized_insertion".into(),
        a: base.clone(),
        b: inserted,
    });
    let mut deleted = base[..3 << 20].to_vec();
    deleted.extend_from_slice(&base[(3 << 20) + (4 << 10)..]);
    v.push(Version {
        name: "localized_deletion".into(),
        a: base.clone(),
        b: deleted,
    });
    let mut replaced = base.clone();
    let patch = text(4 << 10, "replace");
    replaced[5 << 20..(5 << 20) + patch.len()].copy_from_slice(&patch);
    v.push(Version {
        name: "localized_replacement".into(),
        a: base.clone(),
        b: replaced,
    });
    v.push(Version {
        name: "truncation".into(),
        a: base.clone(),
        b: base[..4 << 20].to_vec(),
    });
    v.push(Version {
        name: "complete_rewrite".into(),
        a: base.clone(),
        b: text(8 << 20, "rewrite"),
    });
    v.push(Version {
        name: "large_text_first_ship".into(),
        a: Vec::new(),
        b: text(64 << 20, "large"),
    });
    let random = pseudo_random(8 << 20, "binary");
    let mut random_b = random.clone();
    random_b[1 << 20..(1 << 20) + 4096].copy_from_slice(&pseudo_random(4096, "binary-edit"));
    v.push(Version {
        name: "incompressible_payload_edit".into(),
        a: random,
        b: random_b,
    });
    let mut sparse = vec![0u8; 8 << 20];
    sparse[4 << 20..(4 << 20) + 1024].copy_from_slice(&pseudo_random(1024, "sparse"));
    let mut sparse_b = sparse.clone();
    sparse_b[6 << 20..(6 << 20) + 1024].copy_from_slice(&pseudo_random(1024, "sparse-b"));
    v.push(Version {
        name: "sparse".into(),
        a: sparse,
        b: sparse_b,
    });
    let compressed = zstd::encode_all(&text(16 << 20, "precompressed")[..], 9).unwrap();
    let compressed_b = zstd::encode_all(&text(16 << 20, "precompressed-b")[..], 9).unwrap();
    v.push(Version {
        name: "already_compressed".into(),
        a: compressed,
        b: compressed_b,
    });
    // Fifty offline edits before one catch-up: the source ships the final
    // image once; what differs is how much of it the central already holds.
    let mut edited = base.clone();
    for i in 0..50u32 {
        let at = ((i as usize * 7919) % ((8 << 20) - 8192)) & !0xf;
        let patch = text(1024, &format!("offline-{i}"));
        edited[at..at + patch.len()].copy_from_slice(&patch);
    }
    v.push(Version {
        name: "fifty_offline_edits_one_catch_up".into(),
        a: base,
        b: edited,
    });
    v
}

/// Files from a read-only corpus paired as `(previous, current)` when a
/// sibling `<name>.prev` exists, else measured as a first ship.
fn previous_path(path: &Path) -> std::path::PathBuf {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => path.with_extension(format!("{extension}.prev")),
        None => path.with_extension("prev"),
    }
}

fn corpus_versions(dir: &Path, limit: usize) -> Vec<Version> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_none_or(|e| e != "prev"))
        .collect();
    paths.sort();
    for path in paths.into_iter().take(limit) {
        let Ok(b) = std::fs::read(&path) else {
            continue;
        };
        if b.len() > 256 << 20 {
            continue;
        }
        let prev = previous_path(&path);
        let a = std::fs::read(&prev).unwrap_or_default();
        out.push(Version {
            name: format!(
                "corpus:{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            a,
            b,
        });
    }
    out
}

struct Chunked {
    /// `(hash, compressed bytes, raw length)` per chunk in order.
    chunks: Vec<([u8; 32], Vec<u8>, usize)>,
    hash_ms: f64,
    chunk_ms: f64,
    compress_ms: f64,
    peak: u64,
}

fn split(data: &[u8], strategy: Strategy) -> Chunked {
    let started = Instant::now();
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    match strategy {
        Strategy::WholeCompressed => {
            if !data.is_empty() {
                bounds.push((0, data.len()));
            }
        }
        Strategy::Fixed64K => {
            let mut at = 0;
            while at < data.len() {
                let end = (at + (64 << 10)).min(data.len());
                bounds.push((at, end));
                at = end;
            }
        }
        Strategy::Cdc16K | Strategy::Cdc64K => {
            let params = if strategy == Strategy::Cdc16K {
                CdcParams {
                    min: 4 << 10,
                    average: 16 << 10,
                    max: 64 << 10,
                }
            } else {
                CdcParams {
                    min: 16 << 10,
                    average: 64 << 10,
                    max: 256 << 10,
                }
            };
            let mut chunker = Chunker::new(params);
            let mut at = 0;
            chunker.update(data, |len| {
                bounds.push((at, at + len));
                at += len;
            });
            if let Some(len) = chunker.finish() {
                bounds.push((at, at + len));
            }
        }
    }
    let chunk_ms = ms(started.elapsed());
    let started = Instant::now();
    let hashes: Vec<[u8; 32]> = bounds
        .iter()
        .map(|(s, e)| *blake3::hash(&data[*s..*e]).as_bytes())
        .collect();
    let hash_ms = ms(started.elapsed());
    let started = Instant::now();
    let mut peak = 0u64;
    let chunks = bounds
        .iter()
        .zip(hashes)
        .map(|((s, e), hash)| {
            let compressed = zstd::encode_all(&data[*s..*e], ZSTD_LEVEL)
                .expect("compressing an in-memory chunk");
            peak = peak.max((e - s) as u64 + compressed.len() as u64);
            (hash, compressed, e - s)
        })
        .collect();
    let compress_ms = ms(started.elapsed());
    Chunked {
        chunks,
        hash_ms,
        chunk_ms,
        compress_ms,
        peak,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Measure shipping `b` to a central that holds `a`.
fn measure(name: &str, a: &[u8], b: &[u8], strategy: Strategy) -> Measurement {
    // The central's holdings: compressed chunks of A under the same strategy.
    // Keeping the bytes, not only their hashes, lets the apply measurement
    // reconstruct and verify the exact logical content.
    let held: HashMap<[u8; 32], Vec<u8>> = split(a, strategy)
        .chunks
        .into_iter()
        .map(|(hash, compressed, _)| (hash, compressed))
        .collect();
    let split_b = split(b, strategy);
    let chunks = split_b.chunks.len() as u64;
    let compressed_bytes: u64 = split_b.chunks.iter().map(|(_, c, _)| c.len() as u64).sum();
    let mut reused = 0u64;
    let mut novel = 0u64;
    let mut wire = 0u64;
    // Whole-content never reuses: identical bytes are still shipped.
    let reuse_allowed = strategy != Strategy::WholeCompressed;
    let started = Instant::now();
    let mut records = Vec::with_capacity(split_b.chunks.len());
    for (hash, compressed, raw) in &split_b.chunks {
        let mut record_bytes = MANIFEST_ENTRY;
        if reuse_allowed && held.contains_key(hash) {
            reused += *raw as u64;
        } else {
            novel += *raw as u64;
            record_bytes += compressed.len() as u64;
        }
        wire += record_bytes;
        records.push(record_bytes);
    }
    let stage_ms = ms(started.elapsed());
    // The manifest is durable staging too: without it the receiver cannot
    // order either reused or novel chunks after a restart.
    let staging_bytes = wire;
    // Apply: decompress the staged or previously-held chunk and reassemble
    // the logical content the central indexes through its normal pipeline.
    let started = Instant::now();
    let mut assembled = Vec::with_capacity(b.len());
    for (hash, compressed, raw) in &split_b.chunks {
        let encoded = if reuse_allowed {
            held.get(hash).unwrap_or(compressed)
        } else {
            compressed
        };
        let out = zstd::decode_all(&encoded[..]).expect("decoding a measured chunk");
        assert_eq!(out.len(), *raw, "decoded chunk length changed");
        assembled.extend_from_slice(&out);
    }
    let apply_ms = ms(started.elapsed());
    assert_eq!(
        assembled, b,
        "strategy did not reconstruct the source bytes"
    );
    // Interruption half way through the wire stream, then resume. Only whole
    // records are acknowledged; a record cut by the interruption is resent.
    let started = Instant::now();
    let recovery_bytes = if strategy == Strategy::WholeCompressed {
        wire
    } else {
        let interrupted_at = wire / 2;
        let mut acknowledged = 0u64;
        for record in &records {
            if acknowledged + record > interrupted_at {
                break;
            }
            acknowledged += record;
        }
        wire - acknowledged
    };
    let recovery_ms = ms(started.elapsed());
    let frames = if chunks == 0 {
        0
    } else {
        wire.div_ceil(FRAME_BYTES as u64)
            .max(chunks.div_ceil(FRAME_ROWS))
    };
    Measurement {
        scenario: name.to_string(),
        strategy,
        rejected: false,
        source_bytes: b.len() as u64,
        compressed_bytes,
        transferred_bytes: wire,
        reused_bytes: reused,
        novel_bytes: novel,
        chunks,
        frames,
        hash_ms: split_b.hash_ms,
        chunk_ms: split_b.chunk_ms,
        compress_ms: split_b.compress_ms,
        stage_ms,
        apply_ms,
        peak_working_set_bytes: split_b.peak.max(if strategy == Strategy::WholeCompressed {
            b.len() as u64
        } else {
            0
        }),
        staging_bytes,
        recovery_bytes,
        recovery_ms,
    }
}

fn measure_binary_rejection(data: &[u8], strategy: Strategy) -> Measurement {
    let head = &data[..data.len().min(8 << 10)];
    let started = Instant::now();
    assert!(
        matches!(
            eidos_content::sniff(head),
            eidos_content::Sniff::Binary { .. }
        ),
        "binary fixture was accepted as text"
    );
    let sniff_ms = ms(started.elapsed());
    Measurement {
        scenario: "binary_rejection".into(),
        strategy,
        rejected: true,
        source_bytes: data.len() as u64,
        compressed_bytes: 0,
        transferred_bytes: 0,
        reused_bytes: 0,
        novel_bytes: 0,
        chunks: 0,
        frames: 0,
        hash_ms: 0.0,
        chunk_ms: sniff_ms,
        compress_ms: 0.0,
        stage_ms: 0.0,
        apply_ms: 0.0,
        peak_working_set_bytes: head.len() as u64,
        staging_bytes: 0,
        recovery_bytes: 0,
        recovery_ms: 0.0,
    }
}

/// Run every scenario under every strategy.
pub fn run(corpus: Option<&Path>, corpus_limit: usize) -> Report {
    let mut versions = versions();
    if let Some(dir) = corpus {
        versions.extend(corpus_versions(dir, corpus_limit));
    }
    let mut measurements = Vec::new();
    for v in &versions {
        for strategy in Strategy::ALL {
            measurements.push(measure(&v.name, &v.a, &v.b, strategy));
        }
    }
    let binary = pseudo_random(8 << 20, "binary-rejection");
    for strategy in Strategy::ALL {
        measurements.push(measure_binary_rejection(&binary, strategy));
    }
    let summary = Strategy::ALL
        .iter()
        .map(|s| {
            let rows: Vec<&Measurement> =
                measurements.iter().filter(|m| m.strategy == *s).collect();
            let source: u64 = rows.iter().map(|m| m.source_bytes).sum();
            let transferred: u64 = rows.iter().map(|m| m.transferred_bytes).sum();
            StrategySummary {
                strategy: *s,
                scenarios: rows.len() as u64,
                rejected_scenarios: rows.iter().filter(|row| row.rejected).count() as u64,
                source_bytes: source,
                transferred_bytes: transferred,
                transfer_ratio: if source == 0 {
                    0.0
                } else {
                    transferred as f64 / source as f64
                },
                cpu_ms: rows
                    .iter()
                    .map(|m| m.hash_ms + m.chunk_ms + m.compress_ms + m.stage_ms + m.apply_ms)
                    .sum(),
                frames: rows.iter().map(|m| m.frames).sum(),
                recovery_bytes: rows.iter().map(|m| m.recovery_bytes).sum(),
            }
        })
        .collect();
    Report {
        schema: "eidos-chunking-bakeoff/1",
        frame_bytes: FRAME_BYTES as u64,
        frame_rows: FRAME_ROWS,
        generated_at: eidos_domain::UnixNanos::now().to_rfc3339(),
        measurements,
        summary,
    }
}

/// A compact table for the decision record.
pub fn render_summary(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("| strategy | scenarios | rejected | source MiB | wire MiB | ratio | cpu ms | frames | recovery MiB |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for s in &report.summary {
        out.push_str(&format!(
            "| {} | {} | {} | {:.1} | {:.1} | {:.3} | {:.0} | {} | {:.1} |\n",
            s.strategy.name(),
            s.scenarios,
            s.rejected_scenarios,
            s.source_bytes as f64 / (1u64 << 20) as f64,
            s.transferred_bytes as f64 / (1u64 << 20) as f64,
            s.transfer_ratio,
            s.cpu_ms,
            s.frames,
            s.recovery_bytes as f64 / (1u64 << 20) as f64,
        ));
    }
    out.push('\n');
    out.push_str("| scenario | strategy | source MiB | wire MiB | reused MiB | chunks | frames | cpu ms | recovery MiB |\n");
    out.push_str("|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for m in &report.measurements {
        out.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.2} | {} | {} | {:.0} | {:.2} |\n",
            m.scenario,
            m.strategy.name(),
            m.source_bytes as f64 / (1u64 << 20) as f64,
            m.transferred_bytes as f64 / (1u64 << 20) as f64,
            m.reused_bytes as f64 / (1u64 << 20) as f64,
            m.chunks,
            m.frames,
            m.hash_ms + m.chunk_ms + m.compress_ms + m.stage_ms + m.apply_ms,
            m.recovery_bytes as f64 / (1u64 << 20) as f64,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_strategies_reuse_across_a_localized_edit_and_whole_does_not() {
        let base = text(1 << 20, "t");
        let mut edited = base.clone();
        edited[500_000..500_100].copy_from_slice(&text(100, "e"));
        let whole = measure("x", &base, &edited, Strategy::WholeCompressed);
        let cdc = measure("x", &base, &edited, Strategy::Cdc16K);
        assert_eq!(whole.reused_bytes, 0);
        assert!(cdc.reused_bytes * 10 > cdc.source_bytes * 8, "{cdc:?}");
        assert!(cdc.transferred_bytes < whole.transferred_bytes);
        assert!(cdc.recovery_bytes <= cdc.transferred_bytes);
        assert_eq!(whole.recovery_bytes, whole.transferred_bytes);
    }

    #[test]
    fn a_first_ship_costs_every_strategy_about_the_compressed_size() {
        let b = text(1 << 20, "first");
        for s in Strategy::ALL {
            let m = measure("first", &[], &b, s);
            assert_eq!(m.reused_bytes, 0);
            assert!(m.transferred_bytes >= m.compressed_bytes);
            assert!(m.transferred_bytes < m.source_bytes, "{s:?} {m:?}");
        }
    }

    #[test]
    fn framing_honors_both_byte_and_row_limits() {
        let rows = FRAME_ROWS + 1;
        let wire = rows * MANIFEST_ENTRY;
        assert_eq!(
            wire.div_ceil(FRAME_BYTES as u64)
                .max(rows.div_ceil(FRAME_ROWS)),
            2
        );
    }

    #[test]
    fn interrupted_record_is_resent_and_manifest_is_staged() {
        let b = pseudo_random(256 << 10, "recovery");
        let m = measure("first", &[], &b, Strategy::Fixed64K);
        assert_eq!(m.staging_bytes, m.transferred_bytes);
        assert!(m.recovery_bytes >= m.transferred_bytes / 2);
        assert!(m.recovery_bytes <= m.transferred_bytes);
    }

    #[test]
    fn extensionless_corpus_uses_one_dot_for_the_previous_version() {
        assert_eq!(
            previous_path(Path::new("sample")),
            std::path::PathBuf::from("sample.prev")
        );
        assert_eq!(
            previous_path(Path::new("sample.txt")),
            std::path::PathBuf::from("sample.txt.prev")
        );
    }

    #[test]
    fn binary_input_is_rejected_before_chunking_or_transfer() {
        let binary = pseudo_random(1 << 20, "binary-test");
        for strategy in Strategy::ALL {
            let measurement = measure_binary_rejection(&binary, strategy);
            assert!(measurement.rejected);
            assert_eq!(measurement.chunks, 0);
            assert_eq!(measurement.transferred_bytes, 0);
            assert_eq!(measurement.frames, 0);
        }
    }
}
