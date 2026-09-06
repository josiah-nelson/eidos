//! Durable coordination for settings that intentionally remain in separate
//! ownership files. A journal makes crashes and partial filesystem failures
//! repairable without pretending three replacements are one atomic rename.

use crate::{
    api::{blocking, ApiError, ApiResult},
    api_json::ApiJson,
    device_control::DeviceLimits,
    resource_control::ResourceLimits,
    state::AppState,
};
use axum::{extract::State, routing::get, Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
};
use ts_rs::TS;

const JOURNAL_FILE: &str = "resource-settings-operation.json";
const JOURNAL_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ResourceSettings {
    pub content_workers: u32,
    pub scan_threads: u32,
    pub concurrent_scans: u32,
    pub minimum_free_mib: u32,
    pub readers_per_device: u32,
}

impl ResourceSettings {
    fn validate(&self) -> Result<(), &'static str> {
        if !(1..=crate::content_workers::MAX_WORKERS as u32).contains(&self.content_workers) {
            return Err("content workers must be 1..64");
        }
        self.resource_limits().validate()?;
        self.device_limits().validate()
    }

    fn resource_limits(&self) -> ResourceLimits {
        ResourceLimits {
            scan_threads: self.scan_threads,
            concurrent_scans: self.concurrent_scans,
            minimum_free_mib: self.minimum_free_mib,
        }
    }

    fn device_limits(&self) -> DeviceLimits {
        DeviceLimits {
            readers_per_device: self.readers_per_device,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ResourceComponent {
    ContentWorkers,
    MetadataLimits,
    DeviceReaders,
    OperationJournal,
    JournalCleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ResourceApplyOutcome {
    Current,
    Applied,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Serialize, TS)]
pub struct PendingResourceOperation {
    pub target: ResourceSettings,
    pub completed_components: Vec<ResourceComponent>,
    pub next_component: Option<ResourceComponent>,
    pub failed_component: Option<ResourceComponent>,
    pub error: Option<String>,
    pub cleanup_pending: bool,
}

#[derive(Debug, Clone, Serialize, TS)]
pub struct ResourceSettingsView {
    pub current: ResourceSettings,
    pub outcome: ResourceApplyOutcome,
    pub pending: Option<PendingResourceOperation>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApplyResourceSettings {
    pub settings: ResourceSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationFailure {
    component: ResourceComponent,
    error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationJournal {
    version: u32,
    target: ResourceSettings,
    steps: Vec<ResourceComponent>,
    completed: usize,
    failure: Option<OperationFailure>,
}

pub struct ResourceProfileControl {
    data_dir: PathBuf,
    /// Outermost lock for every manual or coordinated mutation. Component
    /// setters retain their own persistence locks and never acquire this one.
    mutation: Mutex<()>,
    /// Retains a live-process checkpoint/cleanup error that could not itself
    /// be written into the journal, so Activity polling does not erase it.
    last_failure: Mutex<Option<OperationFailure>>,
}

impl ResourceProfileControl {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            mutation: Mutex::new(()),
            last_failure: Mutex::new(None),
        }
    }

    /// Ordinary single-control endpoints share the mutation gate and stop if
    /// a coordinated target still needs repair. This prevents a manual edit
    /// from silently changing the target that restart recovery will replay.
    pub(crate) fn manual_update<T>(
        &self,
        update: impl FnOnce() -> anyhow::Result<T>,
    ) -> Result<T, ApiError> {
        let _mutation = self.mutation.lock();
        match load_journal(&self.data_dir) {
            Ok(Some(_)) => Err(ApiError::conflict(
                "a coordinated resource change is incomplete; repair it before saving an individual control",
            )),
            Ok(None) => {
                let result = update().map_err(|error| ApiError::internal(error.to_string()))?;
                *self.last_failure.lock() = None;
                Ok(result)
            }
            Err(error) => Err(ApiError::internal(format!(
                "cannot verify the coordinated resource journal: {error}"
            ))),
        }
    }

    fn status(&self, state: &AppState) -> Result<ResourceSettingsView, ApiError> {
        let _mutation = self.mutation.lock();
        let journal = load_journal(&self.data_dir).map_err(|error| {
            ApiError::internal(format!(
                "cannot read the coordinated resource journal: {error}"
            ))
        })?;
        let failure = self.last_failure.lock().clone();
        Ok(view(
            state,
            ResourceApplyOutcome::Current,
            journal.as_ref(),
            failure.as_ref(),
        ))
    }

    fn apply(
        &self,
        state: &Arc<AppState>,
        target: ResourceSettings,
    ) -> Result<ResourceSettingsView, ApiError> {
        target.validate().map_err(ApiError::bad_request)?;
        let _mutation = self.mutation.lock();
        if load_journal(&self.data_dir)
            .map_err(|error| ApiError::internal(error.to_string()))?
            .is_some()
        {
            return Err(ApiError::conflict(
                "a coordinated resource change is incomplete; repair it before applying another target",
            ));
        }
        *self.last_failure.lock() = None;

        let current = current_settings(state);
        let steps = ordered_steps(current, target);
        if steps.is_empty() {
            return Ok(view(state, ResourceApplyOutcome::Applied, None, None));
        }
        let mut journal = OperationJournal {
            version: JOURNAL_VERSION,
            target,
            steps,
            completed: 0,
            failure: None,
        };
        if let Err(error) = write_journal(&self.data_dir, &journal) {
            let failure = OperationFailure {
                component: ResourceComponent::OperationJournal,
                error: format!(
                    "no settings changed because the operation journal could not be written: {error}"
                ),
            };
            *self.last_failure.lock() = Some(failure.clone());
            return Ok(view(
                state,
                ResourceApplyOutcome::Failed,
                None,
                Some(&failure),
            ));
        }
        Ok(run_live_operation(
            state,
            &self.data_dir,
            &self.last_failure,
            &mut journal,
        ))
    }

    fn repair(&self, state: &Arc<AppState>) -> Result<ResourceSettingsView, ApiError> {
        let _mutation = self.mutation.lock();
        let Some(mut journal) = load_journal(&self.data_dir).map_err(|error| {
            ApiError::internal(format!(
                "cannot read the coordinated resource journal: {error}"
            ))
        })?
        else {
            *self.last_failure.lock() = None;
            return Ok(view(state, ResourceApplyOutcome::Current, None, None));
        };
        Ok(run_live_operation(
            state,
            &self.data_dir,
            &self.last_failure,
            &mut journal,
        ))
    }
}

fn current_settings(state: &AppState) -> ResourceSettings {
    let resources = state.resources.view().limits;
    let content_workers = if state.content_workers.spawned.load(Ordering::Relaxed) == 0 {
        state.content_worker_count
    } else {
        state.content_workers.workers.load(Ordering::Relaxed)
    };
    ResourceSettings {
        content_workers: content_workers as u32,
        scan_threads: resources.scan_threads,
        concurrent_scans: resources.concurrent_scans,
        minimum_free_mib: resources.minimum_free_mib,
        readers_per_device: state.devices.view().budget.readers_per_device,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Direction {
    Tighten,
    Relax,
}

fn ordered_steps(current: ResourceSettings, target: ResourceSettings) -> Vec<ResourceComponent> {
    let mut steps = Vec::new();
    if current.content_workers != target.content_workers {
        steps.push((
            if target.content_workers < current.content_workers {
                Direction::Tighten
            } else {
                Direction::Relax
            },
            ResourceComponent::ContentWorkers,
        ));
    }
    if current.readers_per_device != target.readers_per_device {
        steps.push((
            if target.readers_per_device < current.readers_per_device {
                Direction::Tighten
            } else {
                Direction::Relax
            },
            ResourceComponent::DeviceReaders,
        ));
    }
    if current.resource_limits() != target.resource_limits() {
        let tightens = target.scan_threads < current.scan_threads
            || target.concurrent_scans < current.concurrent_scans
            || target.minimum_free_mib > current.minimum_free_mib;
        steps.push((
            if tightens {
                Direction::Tighten
            } else {
                Direction::Relax
            },
            ResourceComponent::MetadataLimits,
        ));
    }
    steps.sort_by_key(|(direction, component)| (*direction, component_rank(*component)));
    steps.into_iter().map(|(_, component)| component).collect()
}

fn component_rank(component: ResourceComponent) -> u8 {
    match component {
        ResourceComponent::ContentWorkers => 0,
        ResourceComponent::MetadataLimits => 1,
        ResourceComponent::DeviceReaders => 2,
        ResourceComponent::OperationJournal => 3,
        ResourceComponent::JournalCleanup => 4,
    }
}

fn apply_live_component(
    state: &Arc<AppState>,
    target: ResourceSettings,
    component: ResourceComponent,
) -> anyhow::Result<()> {
    match component {
        ResourceComponent::ContentWorkers => {
            crate::content_workers::resize_workers(state, target.content_workers as usize)?;
        }
        ResourceComponent::MetadataLimits => state.resources.set(target.resource_limits())?,
        ResourceComponent::DeviceReaders => state
            .devices
            .save_limits(&state.data_dir, target.device_limits())?,
        ResourceComponent::OperationJournal | ResourceComponent::JournalCleanup => {
            anyhow::bail!("invalid settings component in operation plan")
        }
    }
    state.content_pause.work.notify_all();
    Ok(())
}

fn run_live_operation(
    state: &Arc<AppState>,
    data_dir: &Path,
    last_failure: &Mutex<Option<OperationFailure>>,
    journal: &mut OperationJournal,
) -> ResourceSettingsView {
    while journal.completed < journal.steps.len() {
        let component = journal.steps[journal.completed];
        if let Err(error) = apply_live_component(state, journal.target, component) {
            let message = error.to_string();
            journal.failure = Some(OperationFailure {
                component,
                error: message.clone(),
            });
            let error = match write_journal(data_dir, journal) {
                Ok(()) => message,
                Err(checkpoint) => {
                    format!("{message}; recording that failure also failed: {checkpoint}")
                }
            };
            let failure = OperationFailure { component, error };
            *last_failure.lock() = Some(failure.clone());
            return view(
                state,
                ResourceApplyOutcome::Partial,
                Some(journal),
                Some(&failure),
            );
        }
        journal.completed += 1;
        journal.failure = None;
        if let Err(error) = write_journal(data_dir, journal) {
            let failure = OperationFailure {
                component: ResourceComponent::OperationJournal,
                error: format!(
                    "{} changed, but progress could not be checkpointed: {error}",
                    component_name(component)
                ),
            };
            journal.failure = Some(failure.clone());
            *last_failure.lock() = Some(failure);
            return view(
                state,
                ResourceApplyOutcome::Partial,
                Some(journal),
                journal.failure.as_ref(),
            );
        }
    }
    if let Err(error) = remove_journal(data_dir) {
        let failure = OperationFailure {
            component: ResourceComponent::JournalCleanup,
            error: format!(
                "all target settings are live, but the completed operation journal could not be removed: {error}"
            ),
        };
        journal.failure = Some(failure.clone());
        *last_failure.lock() = Some(failure);
        let _ = write_journal(data_dir, journal);
        return view(
            state,
            ResourceApplyOutcome::Partial,
            Some(journal),
            journal.failure.as_ref(),
        );
    }
    *last_failure.lock() = None;
    view(state, ResourceApplyOutcome::Applied, None, None)
}

fn component_name(component: ResourceComponent) -> &'static str {
    match component {
        ResourceComponent::ContentWorkers => "content worker limit",
        ResourceComponent::MetadataLimits => "metadata limits",
        ResourceComponent::DeviceReaders => "device reader limit",
        ResourceComponent::OperationJournal => "operation journal",
        ResourceComponent::JournalCleanup => "operation journal cleanup",
    }
}

fn view(
    state: &AppState,
    requested_outcome: ResourceApplyOutcome,
    journal: Option<&OperationJournal>,
    runtime_failure: Option<&OperationFailure>,
) -> ResourceSettingsView {
    let failure =
        runtime_failure.or_else(|| journal.and_then(|operation| operation.failure.as_ref()));
    let pending = journal.map(|operation| PendingResourceOperation {
        target: operation.target,
        completed_components: operation.steps[..operation.completed.min(operation.steps.len())]
            .to_vec(),
        next_component: operation.steps.get(operation.completed).copied(),
        failed_component: failure.map(|failure| failure.component),
        error: failure.map(|failure| failure.error.clone()),
        cleanup_pending: operation.completed >= operation.steps.len(),
    });
    ResourceSettingsView {
        current: current_settings(state),
        outcome: if pending.is_some() && requested_outcome == ResourceApplyOutcome::Current {
            ResourceApplyOutcome::Partial
        } else {
            requested_outcome
        },
        pending,
        error: runtime_failure.map(|failure| failure.error.clone()),
    }
}

fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

fn load_journal(data_dir: &Path) -> anyhow::Result<Option<OperationJournal>> {
    let path = journal_path(data_dir);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_JOURNAL_BYTES,
        "{} exceeds {MAX_JOURNAL_BYTES} bytes",
        path.display()
    );
    let journal: OperationJournal = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid {}: {error}", path.display()))?;
    anyhow::ensure!(
        journal.version == JOURNAL_VERSION,
        "unsupported coordinated resource journal version {}",
        journal.version
    );
    journal.target.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        (1..=3).contains(&journal.steps.len()),
        "invalid coordinated resource step count"
    );
    anyhow::ensure!(
        journal.completed <= journal.steps.len(),
        "invalid completed step count"
    );
    anyhow::ensure!(
        journal.steps.iter().all(|component| matches!(
            component,
            ResourceComponent::ContentWorkers
                | ResourceComponent::MetadataLimits
                | ResourceComponent::DeviceReaders
        )),
        "invalid component in coordinated resource journal"
    );
    anyhow::ensure!(
        journal
            .steps
            .iter()
            .enumerate()
            .all(|(index, component)| !journal.steps[..index].contains(component)),
        "duplicate component in coordinated resource journal"
    );
    Ok(Some(journal))
}

fn write_journal(data_dir: &Path, journal: &OperationJournal) -> anyhow::Result<()> {
    crate::durable_file::replace(&journal_path(data_dir), &serde_json::to_vec(journal)?)?;
    Ok(())
}

fn remove_journal(data_dir: &Path) -> std::io::Result<()> {
    crate::durable_file::remove(&journal_path(data_dir))
}

fn apply_file_component(
    data_dir: &Path,
    target: ResourceSettings,
    component: ResourceComponent,
) -> anyhow::Result<()> {
    match component {
        ResourceComponent::ContentWorkers => crate::content_workers::persist_workers_override(
            data_dir,
            target.content_workers as usize,
        )
        .map_err(Into::into),
        ResourceComponent::MetadataLimits => {
            crate::resource_control::persist_limits(data_dir, target.resource_limits())
        }
        ResourceComponent::DeviceReaders => {
            crate::device_control::persist_limits(data_dir, target.device_limits())
        }
        ResourceComponent::OperationJournal | ResourceComponent::JournalCleanup => {
            anyhow::bail!("invalid settings component in operation plan")
        }
    }
}

/// Finish an interrupted operation before any component reads its standalone
/// file and before background work can start. Invalid/unreadable journals and
/// component/checkpoint failures fail startup closed. A cleanup-only failure
/// leaves a visible, repairable journal but does not block the now-consistent
/// target settings from loading.
pub(crate) fn recover_before_open(data_dir: &Path) -> anyhow::Result<()> {
    let Some(mut journal) = load_journal(data_dir)? else {
        return Ok(());
    };
    while journal.completed < journal.steps.len() {
        let component = journal.steps[journal.completed];
        apply_file_component(data_dir, journal.target, component).map_err(|error| {
            anyhow::anyhow!(
                "recovering coordinated {} failed: {error}",
                component_name(component)
            )
        })?;
        journal.completed += 1;
        journal.failure = None;
        write_journal(data_dir, &journal)?;
    }
    if let Err(error) = remove_journal(data_dir) {
        journal.failure = Some(OperationFailure {
            component: ResourceComponent::JournalCleanup,
            error: format!(
                "all target settings recovered, but the completed operation journal could not be removed: {error}"
            ),
        });
        let _ = write_journal(data_dir, &journal);
        tracing::warn!(error = %error, "coordinated resource settings recovered but journal cleanup remains pending");
    }
    Ok(())
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/resource-settings", get(status).post(apply))
        .route("/resource-settings/repair", axum::routing::post(repair))
}

async fn status(State(state): State<Arc<AppState>>) -> ApiResult<ResourceSettingsView> {
    blocking(move || state.resource_profiles.status(&state).map(ApiJson)).await
}

async fn apply(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ApplyResourceSettings>,
) -> ApiResult<ResourceSettingsView> {
    blocking(move || {
        state
            .resource_profiles
            .apply(&state, body.settings)
            .map(ApiJson)
    })
    .await
}

async fn repair(State(state): State<Arc<AppState>>) -> ApiResult<ResourceSettingsView> {
    blocking(move || state.resource_profiles.repair(&state).map(ApiJson)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceConfig;

    fn open(dir: &Path) -> Arc<AppState> {
        Arc::new(
            AppState::open(&ServiceConfig {
                data_dir: dir.to_path_buf(),
                fleet: false,
                auto_reconcile: false,
                update_check: false,
                ..Default::default()
            })
            .unwrap(),
        )
    }

    #[test]
    fn tightening_steps_precede_relaxing_steps() {
        let current = ResourceSettings {
            content_workers: 4,
            scan_threads: 4,
            concurrent_scans: 1,
            minimum_free_mib: 1024,
            readers_per_device: 2,
        };
        let target = ResourceSettings {
            content_workers: 8,
            scan_threads: 2,
            concurrent_scans: 1,
            minimum_free_mib: 1024,
            readers_per_device: 1,
        };
        assert_eq!(
            ordered_steps(current, target),
            vec![
                ResourceComponent::MetadataLimits,
                ResourceComponent::DeviceReaders,
                ResourceComponent::ContentWorkers,
            ]
        );
    }

    #[test]
    fn unreadable_or_invalid_journal_fails_recovery_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(JOURNAL_FILE), b"not json").unwrap();
        assert!(recover_before_open(dir.path())
            .unwrap_err()
            .to_string()
            .contains("invalid"));

        let target = ResourceSettings {
            content_workers: 2,
            scan_threads: 2,
            concurrent_scans: 1,
            minimum_free_mib: 1024,
            readers_per_device: 2,
        };
        write_journal(
            dir.path(),
            &OperationJournal {
                version: JOURNAL_VERSION,
                target,
                steps: vec![
                    ResourceComponent::ContentWorkers,
                    ResourceComponent::ContentWorkers,
                ],
                completed: 0,
                failure: None,
            },
        )
        .unwrap();
        assert!(recover_before_open(dir.path())
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn partial_component_failure_reports_live_state_and_repair_converges() {
        let dir = tempfile::tempdir().unwrap();
        let state = open(dir.path());
        let target = ResourceSettings {
            content_workers: state.content_worker_count as u32,
            scan_threads: 4,
            concurrent_scans: 1,
            minimum_free_mib: 1024,
            readers_per_device: 1,
        };
        // Metadata tightens first; the device replacement then fails without
        // preventing the durable journal from naming the remaining target.
        std::fs::create_dir(dir.path().join("device-limits.json.tmp")).unwrap();
        let result = state.resource_profiles.apply(&state, target).unwrap();
        assert_eq!(result.outcome, ResourceApplyOutcome::Partial);
        assert_eq!(result.current.scan_threads, 4);
        assert_eq!(result.current.readers_per_device, 2);
        let pending = result.pending.unwrap();
        assert_eq!(
            pending.completed_components,
            vec![ResourceComponent::MetadataLimits]
        );
        assert_eq!(
            pending.next_component,
            Some(ResourceComponent::DeviceReaders)
        );
        assert_eq!(
            pending.failed_component,
            Some(ResourceComponent::DeviceReaders)
        );

        std::fs::remove_dir(dir.path().join("device-limits.json.tmp")).unwrap();
        let repaired = state.resource_profiles.repair(&state).unwrap();
        assert_eq!(repaired.outcome, ResourceApplyOutcome::Applied);
        assert_eq!(repaired.current, target);
        assert!(repaired.pending.is_none());
        assert!(!journal_path(dir.path()).exists());
    }

    #[test]
    fn startup_replays_the_journal_before_component_controls_load() {
        let dir = tempfile::tempdir().unwrap();
        let initial = open(dir.path());
        let current = current_settings(&initial);
        drop(initial);
        let target = ResourceSettings {
            content_workers: 3,
            scan_threads: 2,
            concurrent_scans: 2,
            minimum_free_mib: 2048,
            readers_per_device: 3,
        };
        let journal = OperationJournal {
            version: JOURNAL_VERSION,
            target,
            steps: ordered_steps(current, target),
            completed: 0,
            failure: None,
        };
        write_journal(dir.path(), &journal).unwrap();

        let recovered = open(dir.path());
        assert_eq!(current_settings(&recovered), target);
        assert!(!journal_path(dir.path()).exists());
    }
}
