//! In-memory admission accounting; never probes a source or takes a catalog lock.
//! Topology changes drain old leases before replacing their accounting keys.
//! An unresolved source makes all sources share one conservative budget until
//! topology is known. This fallback is explicit, not guessed disk independence.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub type SourceKey = i64;
pub type Topology = BTreeMap<SourceKey, Option<Vec<String>>>;
const FALLBACK: &str = "unresolved-shared-budget";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Content,
    Scan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    UnknownSource,
    TopologyDraining,
    DeviceAtCapacity,
    InvalidWidth,
}

#[derive(Debug, Default, Clone)]
struct Counts {
    content: u32,
    scan: u32,
    peak: u32,
}

impl Counts {
    fn total(&self) -> u32 {
        self.content + self.scan
    }
}

#[derive(Debug)]
struct State {
    limit: u32,
    topology: Topology,
    admission_keys: BTreeMap<SourceKey, Arc<[String]>>,
    pending: Option<Topology>,
    fallback: bool,
    counts: BTreeMap<String, Counts>,
    leases: u32,
}

impl State {
    fn install(&mut self, topology: Topology) {
        debug_assert_eq!(self.leases, 0);
        self.fallback = topology.values().any(Option::is_none);
        let shared: Arc<[String]> = vec![FALLBACK.to_owned()].into();
        self.admission_keys = topology
            .iter()
            .filter_map(|(id, keys)| {
                if self.fallback {
                    Some((*id, shared.clone()))
                } else {
                    keys.as_ref().map(|keys| (*id, Arc::from(keys.clone())))
                }
            })
            .collect();
        self.topology = topology;
        // Retain peaks for still-present budgets, not every device ever seen.
        // All leases have drained, so obsolete entries cannot own reservations.
        let keys: BTreeSet<_> = self
            .admission_keys
            .values()
            .flat_map(|keys| keys.iter().cloned())
            .collect();
        self.counts.retain(|key, _| keys.contains(key));
        for key in keys {
            self.counts.entry(key).or_default();
        }
    }

    fn keys(&self, source: SourceKey) -> Option<Arc<[String]>> {
        // The per-file hot path clones an Arc, not path/device strings or a Vec.
        self.admission_keys.get(&source).cloned()
    }
}

#[derive(Debug)]
pub struct DeviceBudgets {
    state: Mutex<State>,
}

#[derive(Debug)]
pub struct DeviceLease {
    budgets: Arc<DeviceBudgets>,
    keys: Arc<[String]>,
    kind: WorkKind,
    units: u32,
}

impl DeviceLease {
    /// Enumeration must use this granted width, not its unbounded request.
    pub fn units(&self) -> u32 {
        self.units
    }
}

impl Drop for DeviceLease {
    fn drop(&mut self) {
        let mut state = self.budgets.state.lock().unwrap();
        for key in self.keys.iter() {
            let count = state.counts.get_mut(key).expect("leased device exists");
            match self.kind {
                WorkKind::Content => count.content -= self.units,
                WorkKind::Scan => count.scan -= self.units,
            }
        }
        state.leases -= 1;
        if state.leases == 0 {
            if let Some(topology) = state.pending.take() {
                state.install(topology);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, ts_rs::TS)]
pub struct DeviceSnapshot {
    pub key: String,
    pub sources: Vec<SourceKey>,
    pub content_readers: u32,
    pub scan_threads: u32,
    pub peak_readers: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, ts_rs::TS)]
pub struct BudgetSnapshot {
    pub readers_per_device: u32,
    pub unresolved_shared_fallback: bool,
    pub topology_draining: bool,
    pub devices: Vec<DeviceSnapshot>,
}

impl DeviceBudgets {
    pub fn new(limit: u32) -> Result<Self, &'static str> {
        validate_limit(limit)?;
        Ok(Self {
            state: Mutex::new(State {
                limit,
                topology: BTreeMap::new(),
                admission_keys: BTreeMap::new(),
                pending: None,
                fallback: false,
                counts: BTreeMap::new(),
                leases: 0,
            }),
        })
    }

    pub fn set_limit(&self, limit: u32) -> Result<(), &'static str> {
        validate_limit(limit)?;
        self.state.lock().unwrap().limit = limit;
        Ok(())
    }

    /// Replaces the complete topology snapshot. Removed sources cannot claim
    /// again; active guards still release the exact keys they originally held.
    pub fn set_topology(&self, mut topology: Topology) {
        for keys in topology.values_mut() {
            if let Some(list) = keys {
                list.sort();
                list.dedup();
                if list.is_empty()
                    || list.len() > 64
                    || list.iter().any(|key| key.is_empty() || key == FALLBACK)
                {
                    *keys = None;
                }
            }
        }
        let mut state = self.state.lock().unwrap();
        if topology == state.topology {
            state.pending = None;
        } else if state.leases == 0 {
            state.install(topology);
        } else {
            // No new claims while a mapping change drains; otherwise an old
            // unknown lease could disappear from a newly resolved disk's cap.
            state.pending = Some(topology);
        }
    }

    /// What [`Self::try_reserve`] would grant, without charging anything.
    ///
    /// A refusal here applies to every source sharing the budget, so callers
    /// check this *before* taking another subsystem's reservation. Taking and
    /// immediately dropping one of those would raise its peak counters for
    /// work this gate is what actually held.
    pub fn would_admit(
        &self,
        source: SourceKey,
        kind: WorkKind,
        requested: u32,
    ) -> Result<u32, WaitReason> {
        let state = self.state.lock().unwrap();
        plan(&state, source, kind, requested).map(|(_, units)| units)
    }

    /// Atomically charge all backing devices. Scans may receive less than the
    /// requested width. Content always requests one unit before a one-file claim.
    pub fn try_reserve(
        self: &Arc<Self>,
        source: SourceKey,
        kind: WorkKind,
        requested: u32,
    ) -> Result<DeviceLease, WaitReason> {
        let mut state = self.state.lock().unwrap();
        let (keys, units) = plan(&state, source, kind, requested)?;
        for key in keys.iter() {
            let count = state.counts.get_mut(key).expect("registered device exists");
            match kind {
                WorkKind::Content => count.content += units,
                WorkKind::Scan => count.scan += units,
            }
            count.peak = count.peak.max(count.total());
        }
        state.leases += 1;
        Ok(DeviceLease {
            budgets: self.clone(),
            keys,
            kind,
            units,
        })
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        let state = self.state.lock().unwrap();
        let mut membership: BTreeMap<String, BTreeSet<SourceKey>> = BTreeMap::new();
        for (source, keys) in &state.admission_keys {
            for key in keys.iter() {
                membership.entry(key.clone()).or_default().insert(*source);
            }
        }
        let devices = membership
            .into_iter()
            .map(|(key, sources)| {
                let count = state.counts.get(&key).cloned().unwrap_or_default();
                DeviceSnapshot {
                    key,
                    sources: sources.into_iter().collect(),
                    content_readers: count.content,
                    scan_threads: count.scan,
                    peak_readers: count.peak,
                }
            })
            .collect();
        BudgetSnapshot {
            readers_per_device: state.limit,
            unresolved_shared_fallback: state.fallback,
            topology_draining: state.pending.is_some(),
            devices,
        }
    }
}

/// The single admission decision shared by the charging and read-only paths,
/// so a check can never disagree with the reservation it precedes.
fn plan(
    state: &State,
    source: SourceKey,
    kind: WorkKind,
    requested: u32,
) -> Result<(Arc<[String]>, u32), WaitReason> {
    if !(1..=64).contains(&requested) || (kind == WorkKind::Content && requested != 1) {
        return Err(WaitReason::InvalidWidth);
    }
    if state.pending.is_some() {
        return Err(WaitReason::TopologyDraining);
    }
    let keys = state.keys(source).ok_or(WaitReason::UnknownSource)?;
    let units = keys
        .iter()
        .map(|key| {
            state
                .limit
                .saturating_sub(state.counts.get(key).map(Counts::total).unwrap_or(0))
        })
        .min()
        .unwrap_or(0)
        .min(requested);
    if units == 0 {
        return Err(WaitReason::DeviceAtCapacity);
    }
    Ok((keys, units))
}

fn validate_limit(limit: u32) -> Result<(), &'static str> {
    if !(1..=64).contains(&limit) {
        Err("device reader limit must be 1..64")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology(rows: &[(i64, &[&str])]) -> Topology {
        rows.iter()
            .map(|(id, keys)| (*id, Some(keys.iter().map(|key| key.to_string()).collect())))
            .collect()
    }
    fn fixture(limit: u32, rows: &[(i64, &[&str])]) -> Arc<DeviceBudgets> {
        let budgets = Arc::new(DeviceBudgets::new(limit).unwrap());
        budgets.set_topology(topology(rows));
        budgets
    }

    #[test]
    fn shared_roots_and_multidisk_volumes_charge_every_device() {
        let b = fixture(
            2,
            &[(1, &["a"]), (2, &["a", "b"]), (3, &["b"]), (4, &["c"])],
        );
        let a = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        let both = b.try_reserve(2, WorkKind::Content, 1).unwrap();
        assert_eq!(
            b.try_reserve(1, WorkKind::Content, 1).unwrap_err(),
            WaitReason::DeviceAtCapacity
        );
        let b_only = b.try_reserve(3, WorkKind::Content, 1).unwrap();
        assert!(b.try_reserve(2, WorkKind::Content, 1).is_err());
        assert!(b.try_reserve(4, WorkKind::Content, 1).is_ok());
        drop((a, both, b_only));
        assert!(b.snapshot().devices.iter().all(|d| d.content_readers == 0));
    }

    #[test]
    fn scan_width_and_content_share_the_cap_and_lowering_drains() {
        let b = fixture(3, &[(1, &["a"]), (2, &["a"])]);
        let content = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        let scan = b.try_reserve(2, WorkKind::Scan, 8).unwrap();
        assert_eq!(scan.units(), 2);
        assert!(b.try_reserve(1, WorkKind::Content, 1).is_err());
        b.set_limit(1).unwrap();
        drop(content);
        assert!(b.try_reserve(1, WorkKind::Content, 1).is_err());
        drop(scan);
        assert_eq!(b.try_reserve(1, WorkKind::Scan, 8).unwrap().units(), 1);
        assert_eq!(b.snapshot().devices[0].peak_readers, 3);
    }

    #[test]
    fn unknown_topology_shares_conservative_capacity_and_released_slots() {
        let b = fixture(2, &[(1, &["a"]), (2, &["b"])]);
        let mut map = topology(&[(1, &["a"]), (2, &["b"])]);
        map.insert(3, None);
        b.set_topology(map);
        let first = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        let second = b.try_reserve(2, WorkKind::Content, 1).unwrap();
        assert_eq!(
            b.try_reserve(3, WorkKind::Content, 1).unwrap_err(),
            WaitReason::DeviceAtCapacity
        );
        assert!(b.snapshot().unresolved_shared_fallback);
        assert_eq!(b.snapshot().devices[0].sources, vec![1, 2, 3]);
        drop(first);
        assert!(b.try_reserve(3, WorkKind::Content, 1).is_ok());
        drop(second);
    }

    #[test]
    fn topology_changes_hold_new_work_until_original_leases_drain() {
        let b = fixture(2, &[(1, &["a"]), (2, &["b"])]);
        let a = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        let old_b = b.try_reserve(2, WorkKind::Content, 1).unwrap();
        b.set_topology(topology(&[(1, &["a"]), (2, &["a"])]));
        assert!(b.snapshot().topology_draining);
        assert_eq!(
            b.try_reserve(1, WorkKind::Content, 1).unwrap_err(),
            WaitReason::TopologyDraining
        );
        drop(a);
        assert!(b.try_reserve(2, WorkKind::Content, 1).is_err());
        drop(old_b);
        assert!(!b.snapshot().topology_draining);
        assert_eq!(b.snapshot().devices.len(), 1);
        assert_eq!(b.snapshot().devices[0].sources, vec![1, 2]);
    }

    #[test]
    fn unknown_to_known_and_source_removal_do_not_erase_live_accounting() {
        let b = fixture(1, &[(1, &[])]);
        let held = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        b.set_topology(topology(&[(2, &["a"])]));
        assert!(b.try_reserve(2, WorkKind::Content, 1).is_err());
        drop(held);
        assert_eq!(
            b.try_reserve(1, WorkKind::Content, 1).unwrap_err(),
            WaitReason::UnknownSource
        );
        assert!(b.try_reserve(2, WorkKind::Content, 1).is_ok());
        assert!(!b.snapshot().unresolved_shared_fallback);
    }

    #[test]
    fn topology_updates_coalesce_and_returning_to_original_cancels_drain() {
        let b = fixture(2, &[(1, &["a"])]);
        let held = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        b.set_topology(topology(&[(1, &["b"])]));
        b.set_topology(topology(&[(1, &["c"])]));
        assert!(b.snapshot().topology_draining);
        assert_eq!(b.snapshot().devices[0].key, "a");
        drop(held);
        assert_eq!(b.snapshot().devices[0].key, "c");

        let held = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        b.set_topology(topology(&[(1, &["d"])]));
        b.set_topology(topology(&[(1, &["c"])]));
        assert!(!b.snapshot().topology_draining);
        let second = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        assert_eq!(b.snapshot().devices[0].content_readers, 2);
        drop((held, second));
        assert_eq!(b.snapshot().devices[0].key, "c");
    }

    #[test]
    fn topology_churn_does_not_retain_obsolete_devices() {
        let b = fixture(2, &[(1, &["a"]), (2, &["b"])]);
        drop(b.try_reserve(1, WorkKind::Content, 1).unwrap());
        drop(b.try_reserve(2, WorkKind::Scan, 2).unwrap());
        b.set_topology(topology(&[(2, &["b"])]));
        assert_eq!(b.state.lock().unwrap().counts.len(), 1);
        assert_eq!(b.snapshot().devices[0].peak_readers, 2);
        b.set_topology(Topology::new());
        assert!(b.state.lock().unwrap().counts.is_empty());
        assert_eq!(
            b.try_reserve(2, WorkKind::Content, 1).unwrap_err(),
            WaitReason::UnknownSource
        );
    }

    #[test]
    fn a_denied_multidisk_claim_never_charges_its_other_devices() {
        let b = fixture(1, &[(1, &["a"]), (2, &["a", "b"]), (3, &["b"])]);
        let held = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        assert_eq!(
            b.try_reserve(2, WorkKind::Scan, 1).unwrap_err(),
            WaitReason::DeviceAtCapacity
        );
        assert_eq!(b.snapshot().devices[1].peak_readers, 0);
        let independent = b.try_reserve(3, WorkKind::Content, 1).unwrap();
        drop((held, independent));
    }

    #[test]
    fn invalid_inputs_and_duplicate_devices_do_not_create_capacity() {
        assert!(DeviceBudgets::new(0).is_err());
        let b = fixture(2, &[(1, &["a", "a"])]);
        assert!(b.set_limit(65).is_err());
        assert_eq!(
            b.try_reserve(1, WorkKind::Scan, 0).unwrap_err(),
            WaitReason::InvalidWidth
        );
        assert_eq!(
            b.try_reserve(1, WorkKind::Content, 2).unwrap_err(),
            WaitReason::InvalidWidth
        );
        let held = b.try_reserve(1, WorkKind::Scan, 2).unwrap();
        assert_eq!(b.snapshot().devices[0].scan_threads, 2);
        drop(held);
        assert_eq!(b.snapshot().devices[0].scan_threads, 0);
    }

    #[test]
    fn a_read_only_check_matches_the_reservation_without_charging_anything() {
        let b = fixture(2, &[(1, &["a"]), (2, &["a", "b"]), (3, &["c"])]);
        assert_eq!(b.would_admit(2, WorkKind::Scan, 8), Ok(2));
        assert_eq!(b.would_admit(1, WorkKind::Content, 1), Ok(1));
        assert_eq!(
            b.would_admit(9, WorkKind::Content, 1),
            Err(WaitReason::UnknownSource)
        );
        assert_eq!(
            b.would_admit(1, WorkKind::Content, 2),
            Err(WaitReason::InvalidWidth)
        );
        // Repeated checks must not consume, charge or raise a peak.
        for _ in 0..8 {
            assert_eq!(b.would_admit(1, WorkKind::Content, 1), Ok(1));
        }
        assert!(b.snapshot().devices.iter().all(|d| d.peak_readers == 0));

        let held = b.try_reserve(2, WorkKind::Content, 1).unwrap();
        assert_eq!(b.would_admit(1, WorkKind::Scan, 8), Ok(1));
        let second = b.try_reserve(1, WorkKind::Content, 1).unwrap();
        assert_eq!(b.would_admit(3, WorkKind::Content, 1), Ok(1));
        assert_eq!(
            b.would_admit(1, WorkKind::Content, 1),
            Err(WaitReason::DeviceAtCapacity)
        );
        b.set_topology(topology(&[(1, &["a"])]));
        assert_eq!(
            b.would_admit(1, WorkKind::Content, 1),
            Err(WaitReason::TopologyDraining)
        );
        drop((held, second));
        assert_eq!(b.would_admit(1, WorkKind::Content, 1), Ok(1));
    }

    #[test]
    fn an_unwind_releases_the_lease() {
        let b = fixture(1, &[(1, &["a"])]);
        let copy = b.clone();
        assert!(std::panic::catch_unwind(move || {
            let _lease = copy.try_reserve(1, WorkKind::Content, 1).unwrap();
            panic!("synthetic extraction failure");
        })
        .is_err());
        assert!(b.try_reserve(1, WorkKind::Content, 1).is_ok());
    }

    #[test]
    fn simultaneous_claims_never_oversubscribe_a_shared_device() {
        let b = fixture(3, &[(1, &["a"]), (2, &["a"])]);
        let start = Arc::new(std::sync::Barrier::new(17));
        let acquired = Arc::new(std::sync::Barrier::new(17));
        let release = Arc::new(std::sync::Barrier::new(17));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let (b, start, acquired, release) =
                    (b.clone(), start.clone(), acquired.clone(), release.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let lease = b.try_reserve(1 + i % 2, WorkKind::Content, 1);
                    acquired.wait();
                    release.wait();
                    drop(lease);
                })
            })
            .collect();
        start.wait();
        acquired.wait();
        assert_eq!(b.snapshot().devices[0].content_readers, 3);
        release.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(b.snapshot().devices[0].content_readers, 0);
    }
}
