//! Machine-readable benchmark result format.
//!
//! Benchmarks emit one JSON object per line (`*.jsonl`) using this record so
//! that runs can be diffed and regression-compared. Never put credentials or
//! user file names into `notes`; `target` is a label such as `G:` or
//! `synthetic/medium`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

pub const BENCH_SCHEMA: &str = "eidos-bench/1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchRecord {
    pub schema: String,
    /// Benchmark name, e.g. `profile.walk`, `scan.native`, `query.metadata`.
    pub name: String,
    /// Workload label (not a secret-bearing path).
    pub target: String,
    pub started_at: String,
    pub duration_ms: f64,
    pub ok: bool,
    pub host: String,
    pub build: BuildInfo,
    /// Numeric measurements, e.g. `entries_per_sec`, `p95_ms`.
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    /// Integer counters, e.g. `files`, `directories`, `errors`.
    #[serde(default)]
    pub counters: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BuildInfo {
    pub version: String,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_rev: Option<String>,
}

impl BuildInfo {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_string(),
            git_rev: option_env!("EIDOS_GIT_REV").map(|s| s.to_string()),
        }
    }
}

/// Helper for building a record around a timed section.
pub struct BenchTimer {
    record: BenchRecord,
    start: Instant,
}

impl BenchTimer {
    pub fn start(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            record: BenchRecord {
                schema: BENCH_SCHEMA.to_string(),
                name: name.into(),
                target: target.into(),
                started_at: crate::time::UnixNanos::now().to_rfc3339(),
                duration_ms: 0.0,
                ok: true,
                host: hostname(),
                build: BuildInfo::current(),
                metrics: BTreeMap::new(),
                counters: BTreeMap::new(),
                notes: Vec::new(),
            },
            start: Instant::now(),
        }
    }

    pub fn metric(&mut self, k: &str, v: f64) -> &mut Self {
        self.record.metrics.insert(k.to_string(), v);
        self
    }

    pub fn counter(&mut self, k: &str, v: u64) -> &mut Self {
        self.record.counters.insert(k.to_string(), v);
        self
    }

    pub fn note(&mut self, n: impl Into<String>) -> &mut Self {
        self.record.notes.push(n.into());
        self
    }

    pub fn fail(&mut self) -> &mut Self {
        self.record.ok = false;
        self
    }

    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    pub fn finish(mut self) -> BenchRecord {
        self.record.duration_ms = self.elapsed_ms();
        self.record
    }
}

/// Simple latency histogram used by query benchmarks to compute percentiles.
#[derive(Debug, Default, Clone)]
pub struct LatencySamples {
    samples_us: Vec<u64>,
}

impl LatencySamples {
    pub fn push(&mut self, d: std::time::Duration) {
        self.samples_us.push(d.as_micros() as u64);
    }

    pub fn len(&self) -> usize {
        self.samples_us.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples_us.is_empty()
    }

    pub fn percentile_ms(&self, p: f64) -> f64 {
        if self.samples_us.is_empty() {
            return 0.0;
        }
        let mut s = self.samples_us.clone();
        s.sort_unstable();
        let rank = ((p / 100.0) * (s.len() as f64 - 1.0)).round() as usize;
        s[rank.min(s.len() - 1)] as f64 / 1000.0
    }

    pub fn mean_ms(&self) -> f64 {
        if self.samples_us.is_empty() {
            return 0.0;
        }
        self.samples_us.iter().sum::<u64>() as f64 / self.samples_us.len() as f64 / 1000.0
    }

    /// Write p50/p95/p99/mean/max into a record under `prefix`.
    pub fn record_into(&self, t: &mut BenchTimer, prefix: &str) {
        t.metric(&format!("{prefix}.p50_ms"), self.percentile_ms(50.0));
        t.metric(&format!("{prefix}.p95_ms"), self.percentile_ms(95.0));
        t.metric(&format!("{prefix}.p99_ms"), self.percentile_ms(99.0));
        t.metric(&format!("{prefix}.mean_ms"), self.mean_ms());
        t.metric(&format!("{prefix}.max_ms"), self.percentile_ms(100.0));
        t.counter(&format!("{prefix}.samples"), self.len() as u64);
    }
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_to_single_line() {
        let mut t = BenchTimer::start("test.bench", "synthetic");
        t.counter("files", 3)
            .metric("entries_per_sec", 1.5)
            .note("hello");
        let r = t.finish();
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains('\n'));
        let back: BenchRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.schema, BENCH_SCHEMA);
        assert_eq!(back.counters["files"], 3);
    }

    #[test]
    fn percentiles() {
        let mut l = LatencySamples::default();
        for i in 1..=100 {
            l.push(std::time::Duration::from_millis(i));
        }
        assert!((l.percentile_ms(50.0) - 50.0).abs() < 1.5);
        assert!((l.percentile_ms(95.0) - 95.0).abs() < 1.5);
        assert!((l.percentile_ms(100.0) - 100.0).abs() < 0.01);
    }
}
