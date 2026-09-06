//! Cached topology feeds in-memory admission. No OS calls occur in a claim,
//! scan-admission loop or HTTP snapshot; only the one background probe may block.

use crate::{
    background_probe::BackgroundProbe,
    device_budget::{
        BudgetSnapshot, DeviceBudgets, DeviceLease, SourceKey, Topology, WaitReason, WorkKind,
    },
    device_topology,
};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub type SourceRoots = BTreeMap<SourceKey, PathBuf>;
type Results = BTreeMap<SourceKey, Result<Vec<String>, String>>;
const REFRESH: Duration = Duration::from_secs(30);
const MAX_AGE: Duration = Duration::from_secs(90);
const SETTINGS_FILE: &str = "device-limits.json";

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, ts_rs::TS)]
#[serde(deny_unknown_fields)]
pub struct DeviceLimits {
    pub readers_per_device: u32,
}

impl DeviceLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if (1..=64).contains(&self.readers_per_device) {
            Ok(())
        } else {
            Err("device reader limit must be 1..64")
        }
    }
}

#[derive(Clone)]
struct ProbeBatch {
    revision: u64,
    started: Instant,
    results: Results,
}

#[derive(Default)]
struct State {
    roots: SourceRoots,
    revision: u64,
    sample_started: Option<Instant>,
    stale: bool,
    errors: BTreeMap<SourceKey, String>,
}

pub struct DeviceControl {
    budgets: Arc<DeviceBudgets>,
    state: Mutex<State>,
    probe: Arc<BackgroundProbe<ProbeBatch>>,
    settings_write: Mutex<()>,
}

#[derive(Debug, Clone, serde::Serialize, ts_rs::TS)]
pub struct DeviceView {
    pub budget: BudgetSnapshot,
    pub sample_age_s: Option<u64>,
    pub stale: bool,
    pub source_errors: BTreeMap<SourceKey, String>,
    pub source_roots: BTreeMap<SourceKey, String>,
}

impl DeviceControl {
    pub fn new(limit: u32) -> Result<Self, &'static str> {
        Ok(Self {
            budgets: Arc::new(DeviceBudgets::new(limit)?),
            state: Mutex::new(State {
                stale: true,
                ..State::default()
            }),
            probe: Arc::new(BackgroundProbe::default()),
            settings_write: Mutex::new(()),
        })
    }

    pub fn load(data_dir: &std::path::Path) -> anyhow::Result<Self> {
        use std::io::Read;
        let limits = match std::fs::File::open(data_dir.join(SETTINGS_FILE)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(4097).read_to_end(&mut bytes)?;
                anyhow::ensure!(bytes.len() <= 4096, "device limits file exceeds 4096 bytes");
                serde_json::from_slice::<DeviceLimits>(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DeviceLimits {
                readers_per_device: 2,
            },
            Err(error) => return Err(error.into()),
        };
        limits.validate().map_err(anyhow::Error::msg)?;
        Self::new(limits.readers_per_device).map_err(anyhow::Error::msg)
    }

    /// Persist first without holding an admission mutex across disk I/O.
    pub fn save_limits(
        &self,
        data_dir: &std::path::Path,
        limits: DeviceLimits,
    ) -> anyhow::Result<()> {
        limits.validate().map_err(anyhow::Error::msg)?;
        let _write = self.settings_write.lock();
        persist_limits(data_dir, limits)?;
        self.set_limit(limits.readers_per_device)
            .map_err(anyhow::Error::msg)
    }

    /// Call with the authoritative local-source list, never fleet replicas.
    /// A new/root-changed source invalidates the whole topology until a matching
    /// sample arrives. Any old leases drain against their original keys.
    pub fn set_sources(&self, roots: SourceRoots) {
        let mut state = self.state.lock();
        if state.roots == roots {
            return;
        }
        state.roots = roots;
        state.revision = state.revision.wrapping_add(1);
        state.sample_started = None;
        state.errors.clear();
        state.stale = true;
        self.budgets.set_topology(unknown(&state.roots));
    }

    pub fn set_limit(&self, limit: u32) -> Result<(), &'static str> {
        self.budgets.set_limit(limit)
    }

    /// Demand-driven: call when work needs admission or the operator requests
    /// topology diagnostics, not from an unconditional quiet-idle timer.
    pub fn refresh(&self) {
        self.refresh_with(device_topology::probe_root);
    }

    fn refresh_with(
        &self,
        probe: impl Fn(&std::path::Path) -> Result<Vec<String>, String> + Send + 'static,
    ) {
        let cached = self.probe.snapshot();
        let completed_revision = cached
            .as_ref()
            .and_then(|(_, result)| result.as_ref().ok())
            .map(|batch| batch.revision);
        if let Some((_, result)) = cached {
            match result {
                Ok(batch) => self.accept(batch),
                Err(error) => {
                    let mut state = self.state.lock();
                    state.errors = state.roots.keys().map(|id| (*id, error.clone())).collect();
                    state.stale = true;
                    self.budgets.set_topology(unknown(&state.roots));
                }
            }
        }
        self.expire();
        let state = self.state.lock();
        if state.roots.is_empty() {
            return;
        }
        let roots = state.roots.clone();
        let revision = state.revision;
        // A changed source should refresh immediately once the previous probe
        // finishes, but must never replace a still-running/stuck worker.
        // Errors retain the retry TTL too; an unavailable root must not turn
        // every admission tick into a newly spawned failing probe.
        let ttl = if completed_revision.is_some_and(|done| done != revision) {
            Duration::ZERO
        } else {
            REFRESH
        };
        drop(state);
        self.probe.refresh(ttl, move || {
            let started = Instant::now();
            let results = roots.into_iter().map(|(id, root)| {
                let result = if started.elapsed() > MAX_AGE {
                    Err("topology batch exceeded its freshness window before this root was probed".into())
                } else { probe(&root) };
                (id, result)
            }).collect();
            Ok(ProbeBatch { revision, started, results })
        });
    }

    fn accept(&self, batch: ProbeBatch) {
        let mut state = self.state.lock();
        if batch.revision != state.revision || state.sample_started == Some(batch.started) {
            return;
        }
        // Use the start time, not just completion time: a slow batch must not
        // make its early samples look fresh after minutes in another syscall.
        state.sample_started = Some(batch.started);
        state.stale = batch.started.elapsed() > MAX_AGE;
        state.errors = batch
            .results
            .iter()
            .filter_map(|(id, result)| result.as_ref().err().map(|error| (*id, error.clone())))
            .collect();
        let topology = if state.stale {
            unknown(&state.roots)
        } else {
            state
                .roots
                .keys()
                .map(|id| {
                    (
                        *id,
                        batch.results.get(id).and_then(|r| r.as_ref().ok()).cloned(),
                    )
                })
                .collect()
        };
        self.budgets.set_topology(topology);
    }

    fn expire(&self) {
        let mut state = self.state.lock();
        if !state.stale && state.sample_started.is_none_or(|at| at.elapsed() > MAX_AGE) {
            state.stale = true;
            self.budgets.set_topology(unknown(&state.roots));
        }
    }

    /// Read-only capacity check; see [`DeviceBudgets::would_admit`].
    pub fn would_admit(
        &self,
        source: SourceKey,
        kind: WorkKind,
        width: u32,
    ) -> Result<u32, WaitReason> {
        self.expire();
        self.budgets.would_admit(source, kind, width)
    }

    pub fn try_reserve(
        &self,
        source: SourceKey,
        kind: WorkKind,
        width: u32,
    ) -> Result<DeviceLease, WaitReason> {
        self.expire();
        self.budgets.try_reserve(source, kind, width)
    }

    pub fn view(&self) -> DeviceView {
        self.expire();
        let state = self.state.lock();
        DeviceView {
            budget: self.budgets.snapshot(),
            sample_age_s: state.sample_started.map(|at| at.elapsed().as_secs()),
            stale: state.stale,
            source_errors: state.errors.clone(),
            source_roots: state
                .roots
                .iter()
                .map(|(id, root)| (*id, root.to_string_lossy().into_owned()))
                .collect(),
        }
    }
}

/// Write the standalone device file using the same durable replacement used
/// by both the ordinary endpoint and coordinated recovery.
pub(crate) fn persist_limits(
    data_dir: &std::path::Path,
    limits: DeviceLimits,
) -> anyhow::Result<()> {
    limits.validate().map_err(anyhow::Error::msg)?;
    crate::durable_file::replace(&data_dir.join(SETTINGS_FILE), &serde_json::to_vec(&limits)?)?;
    Ok(())
}

fn unknown(roots: &SourceRoots) -> Topology {
    roots.keys().map(|id| (*id, None)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    fn roots() -> SourceRoots {
        [
            (1, PathBuf::from("synthetic-first")),
            (2, PathBuf::from("synthetic-second")),
        ]
        .into()
    }

    fn batch(control: &DeviceControl, started: Instant) -> ProbeBatch {
        ProbeBatch {
            revision: control.state.lock().revision,
            started,
            results: [(1, Ok(vec!["a".into()])), (2, Ok(vec!["b".into()]))].into(),
        }
    }

    #[test]
    fn changed_roots_reject_a_completed_old_probe() {
        let c = DeviceControl::new(1).unwrap();
        c.set_sources(roots());
        let old = batch(&c, Instant::now());
        let mut changed = roots();
        changed.insert(1, "replacement-root".into());
        c.set_sources(changed);
        c.accept(old);
        assert!(c.view().stale);
        assert!(c.view().budget.unresolved_shared_fallback);
        c.accept(batch(&c, Instant::now()));
        assert!(!c.view().stale);
        assert_eq!(c.view().budget.devices.len(), 2);
    }

    #[test]
    fn stale_samples_fall_back_only_after_live_leases_drain() {
        let c = DeviceControl::new(1).unwrap();
        c.set_sources(roots());
        c.accept(batch(&c, Instant::now()));
        let a = c.try_reserve(1, WorkKind::Content, 1).unwrap();
        let b = c.try_reserve(2, WorkKind::Content, 1).unwrap();
        c.state.lock().sample_started = Some(Instant::now() - MAX_AGE - Duration::from_secs(1));
        assert_eq!(
            c.try_reserve(1, WorkKind::Content, 1).unwrap_err(),
            WaitReason::TopologyDraining
        );
        drop(a);
        assert!(c.view().budget.topology_draining);
        drop(b);
        let a = c.try_reserve(1, WorkKind::Content, 1).unwrap();
        assert!(c.try_reserve(2, WorkKind::Content, 1).is_err());
        assert!(c.view().stale);
        assert!(c.view().budget.unresolved_shared_fallback);
        drop(a);
    }

    #[test]
    fn a_slow_batch_is_not_fresh_merely_because_it_just_completed() {
        let c = DeviceControl::new(1).unwrap();
        c.set_sources(roots());
        c.accept(batch(&c, Instant::now() - MAX_AGE - Duration::from_secs(1)));
        assert!(c.view().stale);
        assert!(c.view().budget.unresolved_shared_fallback);
    }

    #[tokio::test]
    async fn stuck_probe_never_multiplies_and_claims_do_not_probe() {
        let c = DeviceControl::new(1).unwrap();
        c.set_sources(roots());
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(Some(blocked));
        c.refresh_with(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            if let Some(blocked) = blocked.lock().take() {
                blocked.recv().unwrap();
            }
            Ok(vec!["a".into()])
        });
        assert!(c.probe.cached(Duration::from_millis(20)).await.is_err());
        for _ in 0..100 {
            c.refresh_with(|_| panic!("replacement worker launched"));
            drop(c.try_reserve(1, WorkKind::Content, 1).unwrap());
            assert!(c.view().budget.unresolved_shared_fallback);
        }
        release.send(()).unwrap();
        let result = c.probe.cached(Duration::from_secs(2)).await.unwrap();
        c.accept(result);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!c.view().budget.unresolved_shared_fallback);
    }

    #[test]
    fn probe_failures_keep_the_retry_ttl_and_do_not_spawn_on_every_request() {
        let mut c = DeviceControl::new(1).unwrap();
        c.set_sources(roots());
        c.probe = Arc::new(BackgroundProbe::seeded(Err("probe worker failed".into())));
        for _ in 0..100 {
            c.refresh_with(|_| panic!("error TTL was bypassed"));
        }
        assert!(c.view().stale);
        assert_eq!(c.view().source_errors.len(), 2);
        assert!(c.view().budget.unresolved_shared_fallback);
    }
}
