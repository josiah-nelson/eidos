//! Operator controls for background content work, and the one place that
//! decides what "content state" means.
//!
//! Two switches gate extraction, and they are deliberately different:
//!
//! - `content_enabled` (`--no-content`) is a *process* setting. It is chosen
//!   at start-up, is not persisted, and turns the whole pipeline off.
//! - The pause flag here is an *operator* control taken while the service
//!   runs (`POST /api/content/pause`, `eidos content pause`). It stops new
//!   job **claims** only: a batch a worker already holds runs to completion
//!   and is committed and published normally, so pausing never abandons or
//!   corrupts work in flight.
//!
//! ## Persistence
//!
//! A pause survives a restart. It is recorded in `content-pause.json` in the
//! data directory, written to a temporary file and renamed into place, and
//! removed on resume; the file's presence is the flag. The catalog has no
//! settings table, and the marker mirrors the content index's `rebuild.json`
//! so both durable operator/recovery states live next to the data they
//! describe. The alternative — a pause that lapses at the next start — was
//! rejected: an operator who pauses because a volume is busy or a share is
//! saturated would silently get the load back after a service restart or a
//! crash, which is exactly when they least expect it. Resuming is always
//! explicit.
//!
//! [`content_status`] derives the state that `/api/health`, `/api/activity`,
//! `eidos activity`, and the web Activity page all report, so those four can
//! never disagree.

use crate::api::{ApiError, ApiResult};
use crate::api_json::ApiJson;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::{get, post};
use axum::Router;
use eidos_search::content::{RebuildPhase, RebuildStatus};
use parking_lot::{Mutex, MutexGuard};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use ts_rs::TS;

/// Durable marker for a paused content pipeline, in the data directory.
pub const PAUSE_MARKER: &str = "content-pause.json";

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/content/status", get(status))
        .route("/content/pause", post(pause))
        .route("/content/resume", post(resume))
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct PauseMarker {
    paused_at_unix_s: u64,
}

fn now_unix_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The global "stop claiming content jobs" flag and its durable marker.
#[derive(Debug)]
pub struct ContentPause {
    path: PathBuf,
    /// Serialises durable control transitions with worker claims, queue
    /// top-up, and scan registration. Holding this gate is the admission
    /// linearization point:
    /// once a pause response is produced, no worker can still be between its
    /// gate check and the catalog claim; once a scan is registered, no worker
    /// can claim that source from a stale scan snapshot.
    admission: Mutex<()>,
    paused: AtomicBool,
    /// Wall-clock second the pause was requested; meaningless when running.
    since_unix_s: AtomicU64,
}

impl ContentPause {
    /// Read the marker in `data_dir`. A marker that exists but cannot be
    /// parsed still means paused — the timestamp inside it is advisory.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(PAUSE_MARKER);
        let (paused, since) = match std::fs::metadata(&path) {
            Ok(_) => {
                let parsed: Option<PauseMarker> = std::fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                let since = parsed
                    .map(|m| m.paused_at_unix_s)
                    .unwrap_or_else(now_unix_s);
                tracing::warn!(
                    since_unix_s = since,
                    "content extraction is paused by a persisted operator request; \
                     resume it with `eidos content resume`"
                );
                (true, since)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && data_dir.is_dir() => (false, 0),
            Err(e) => {
                // An unreadable data directory must not silently re-enable
                // source I/O. Fail closed until an operator can inspect the
                // marker or explicitly resume successfully.
                let since = now_unix_s();
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "could not inspect the content-pause marker; keeping extraction paused"
                );
                (true, since)
            }
        };
        Self {
            path,
            admission: Mutex::new(()),
            paused: AtomicBool::new(paused),
            since_unix_s: AtomicU64::new(since),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn paused_since_unix_s(&self) -> Option<u64> {
        self.is_paused()
            .then(|| self.since_unix_s.load(Ordering::Relaxed))
    }

    /// Pause or resume claiming. Returns whether the flag actually changed.
    ///
    /// The durable marker is updated *first*: if it cannot be written the
    /// call fails and the in-memory flag is untouched, so what the operator
    /// is told and what a restart will do never diverge. Both directions are
    /// idempotent.
    pub fn set_paused(&self, paused: bool) -> std::io::Result<bool> {
        let _admission = self.admission.lock();
        self.set_paused_admitted(paused)
    }

    /// Enter the shared content-work/control/scan admission gate.
    pub(crate) fn admission_guard(&self) -> MutexGuard<'_, ()> {
        self.admission.lock()
    }

    /// Change the durable state while the caller holds [`Self::admission`].
    /// Keeping this separate lets an API response derive its status before a
    /// concurrent opposite control can take effect.
    pub(crate) fn set_paused_admitted(&self, paused: bool) -> std::io::Result<bool> {
        if paused == self.is_paused() {
            return Ok(false);
        }
        let at = now_unix_s();
        if paused {
            let tmp = self.path.with_extension("json.tmp");
            let body = serde_json::to_vec(&PauseMarker {
                paused_at_unix_s: at,
            })
            .expect("pause marker");
            std::fs::write(&tmp, body)?;
            std::fs::rename(&tmp, &self.path)?;
            self.since_unix_s.store(at, Ordering::Relaxed);
        } else {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        self.paused.store(paused, Ordering::Release);
        tracing::info!(paused, "content claiming switched");
        Ok(true)
    }
}

/// What the workers are doing with respect to claiming new jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ContentFlow {
    /// `--no-content`: this process extracts nothing.
    Disabled,
    /// Claiming has stopped and no batch is in flight.
    Stopped,
    /// Claiming has stopped but workers are still finishing claimed batches.
    Draining,
    /// Claiming is allowed but there is nothing to claim: the queue is empty
    /// or every source is at its concurrency budget.
    Waiting,
    /// Workers are extracting.
    Running,
}

/// How complete content *search* is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ContentSearchState {
    /// The content index reflects every stored chunk.
    Ready,
    /// A rebuild from stored chunks is pending or running; results are partial.
    Rebuilding,
    /// The last rebuild failed; results stay partial until it runs again.
    Failed,
    /// Extraction is off for this process, so coverage will not grow.
    Disabled,
}

/// The content state shared by `/api/health`, `/api/activity`, the CLI, and
/// the web UI.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(optional_fields)]
pub struct ContentStatusView {
    pub search: ContentSearchState,
    /// One line describing `search` and `flow` together.
    pub detail: String,
    /// Process-level switch (`--no-content` clears it).
    pub enabled: bool,
    /// Operator pause; persisted across restarts.
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_since_unix_s: Option<u64>,
    pub flow: ContentFlow,
    pub flow_reason: String,
    /// Source-budget units held by workers: one per batch being extracted.
    pub in_flight: u32,
    pub rebuild: RebuildStatus,
}

/// Derive the current content state. Cheap: atomics, one budget-table lock,
/// and the rebuild mutex — no catalog or index reads. The function does not
/// acquire the admission gate itself; control endpoints deliberately call it
/// while holding that gate so their response describes their own transition.
pub fn content_status(state: &AppState) -> ContentStatusView {
    let enabled = state.content_enabled.load(Ordering::Relaxed);
    let paused = state.content_pause.is_paused();
    let rebuild = state.content_index.rebuild_status();
    let rebuilding = matches!(rebuild.phase, RebuildPhase::Pending | RebuildPhase::Running);
    let in_flight = state.content_budgets().reserved_total();
    let busy = in_flight > 0;

    // Order matters: the reason an operator is shown is the one they can act
    // on. `--no-content` outranks everything, and a rebuild outranks a pause:
    // resuming while the rebuild owns the writer still claims nothing, so
    // saying "paused" would send the operator after the wrong switch.
    let (flow, flow_reason) = if !enabled {
        (
            ContentFlow::Disabled,
            "content extraction is disabled for this process (--no-content)".to_string(),
        )
    } else if rebuilding {
        if busy {
            (
                ContentFlow::Draining,
                "the content index is being rebuilt: no new jobs are claimed until it finishes"
                    .to_string(),
            )
        } else {
            (
                ContentFlow::Waiting,
                "the content index is being rebuilt: claiming resumes when it finishes".to_string(),
            )
        }
    } else if paused {
        if busy {
            (
                ContentFlow::Draining,
                format!(
                    "paused: no new jobs are claimed; {in_flight} batch(es) already claimed \
                     finish and publish normally"
                ),
            )
        } else {
            (
                ContentFlow::Stopped,
                "paused: no jobs are claimed and nothing is in flight".to_string(),
            )
        }
    } else if busy {
        (
            ContentFlow::Running,
            format!("{in_flight} batch(es) in flight"),
        )
    } else {
        (
            ContentFlow::Waiting,
            "nothing to claim: the queue is empty or every source is at its concurrency budget"
                .to_string(),
        )
    };

    let search = if rebuilding {
        ContentSearchState::Rebuilding
    } else if rebuild.phase == RebuildPhase::Failed {
        ContentSearchState::Failed
    } else if !enabled {
        ContentSearchState::Disabled
    } else {
        ContentSearchState::Ready
    };
    let head = match search {
        ContentSearchState::Ready => "content search is ready".to_string(),
        ContentSearchState::Rebuilding => format!(
            "content search is partial: rebuilding the index from stored chunks \
             ({} of {} documents, {} s)",
            rebuild.docs,
            rebuild.chunks,
            rebuild.elapsed_ms / 1000
        ),
        ContentSearchState::Failed => format!(
            "content search is partial: the last index rebuild failed ({}); it runs \
             again at the next start",
            rebuild.error.as_deref().unwrap_or("unknown error")
        ),
        ContentSearchState::Disabled => {
            "content search covers only what was already indexed".to_string()
        }
    };

    ContentStatusView {
        search,
        detail: format!("{head}; {flow_reason}"),
        enabled,
        paused,
        paused_since_unix_s: state.content_pause.paused_since_unix_s(),
        flow,
        flow_reason,
        in_flight,
        rebuild,
    }
}

async fn status(State(st): State<Arc<AppState>>) -> ApiResult<ContentStatusView> {
    Ok(ApiJson(content_status(&st)))
}

async fn pause(State(st): State<Arc<AppState>>) -> ApiResult<ContentStatusView> {
    // The durable filesystem mutation and admission wait are synchronous.
    // Keep both off an async runtime worker, like the other operator
    // mutations in the service.
    crate::api::blocking(move || set(&st, true)).await
}

async fn resume(State(st): State<Arc<AppState>>) -> ApiResult<ContentStatusView> {
    crate::api::blocking(move || set(&st, false)).await
}

/// Switch the pause and answer with the resulting state, so a caller that
/// pauses learns *in the same response* whether workers are already stopped
/// or still draining a claimed batch.
fn set(st: &AppState, paused: bool) -> ApiResult<ContentStatusView> {
    // Keep the gate through status derivation. The response therefore names
    // the state this request produced, and a worker cannot pass its gate and
    // claim after a completed pause reports `stopped`.
    let _admission = st.content_pause.admission_guard();
    st.content_pause.set_paused_admitted(paused).map_err(|e| {
        ApiError::internal(format!(
            "the pause marker could not be {}: {e}",
            if paused { "written" } else { "removed" }
        ))
    })?;
    Ok(ApiJson(content_status(st)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_workers::reserve_and_claim;
    use crate::scanner::ScanProgress;
    use crate::ServiceConfig;
    use eidos_catalog::jobs::NewJob;
    use eidos_catalog::NewSource;
    use eidos_domain::{JobStage, Priority, SourceKind};
    use std::sync::Barrier;

    #[test]
    fn the_marker_round_trips_and_both_directions_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = ContentPause::load(dir.path());
        assert!(!p.is_paused());
        assert_eq!(p.paused_since_unix_s(), None);

        assert!(p.set_paused(true).unwrap(), "first pause changed the flag");
        assert!(!p.set_paused(true).unwrap(), "pausing twice is a no-op");
        assert!(p.is_paused());
        assert!(p.paused_since_unix_s().is_some());
        assert!(dir.path().join(PAUSE_MARKER).exists());

        // A fresh load (a restart) sees the pause.
        let reloaded = ContentPause::load(dir.path());
        assert!(reloaded.is_paused());
        assert_eq!(reloaded.paused_since_unix_s(), p.paused_since_unix_s());

        assert!(reloaded.set_paused(false).unwrap());
        assert!(!reloaded.set_paused(false).unwrap());
        assert!(!dir.path().join(PAUSE_MARKER).exists());
        assert!(!ContentPause::load(dir.path()).is_paused());
    }

    #[test]
    fn a_torn_marker_still_means_paused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PAUSE_MARKER), b"{\"paused_at_unix").unwrap();
        let p = ContentPause::load(dir.path());
        assert!(p.is_paused());
        assert!(
            p.paused_since_unix_s().is_some_and(|s| s > 0),
            "an unreadable timestamp falls back to now"
        );
    }

    #[test]
    fn a_pause_that_cannot_be_recorded_does_not_change_the_flag() {
        // The marker's directory is gone, so the temporary file cannot be
        // written. Reporting "paused" here would promise a restart behaviour
        // the service could not deliver.
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let p = ContentPause::load(&data);
        std::fs::remove_dir_all(&data).unwrap();

        assert!(p.set_paused(true).is_err());
        assert!(!p.is_paused(), "a failed write leaves claiming enabled");
    }

    #[test]
    fn concurrent_controls_are_idempotent_and_match_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let pause = Arc::new(ContentPause::load(dir.path()));
        let ready = Arc::new(Barrier::new(17));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let pause = pause.clone();
            let ready = ready.clone();
            threads.push(std::thread::spawn(move || {
                ready.wait();
                pause.set_paused(true)
            }));
        }
        ready.wait();
        let changed = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .filter(|changed| *changed)
            .count();
        assert_eq!(changed, 1, "exactly one overlapping pause changes state");
        assert!(pause.is_paused());

        // Opposite controls may finish in either order, but the in-memory
        // result and the durable state must always describe that same order.
        let ready = Arc::new(Barrier::new(3));
        let resume = {
            let pause = pause.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                ready.wait();
                pause.set_paused(false).unwrap();
            })
        };
        let repause = {
            let pause = pause.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                ready.wait();
                pause.set_paused(true).unwrap();
            })
        };
        ready.wait();
        resume.join().unwrap();
        repause.join().unwrap();

        let marker_exists = dir.path().join(PAUSE_MARKER).exists();
        assert_eq!(pause.is_paused(), marker_exists);
        assert_eq!(ContentPause::load(dir.path()).is_paused(), marker_exists);
        assert!(!dir.path().join("content-pause.json.tmp").exists());
    }

    fn state_with_queued_job() -> (tempfile::TempDir, Arc<AppState>, eidos_domain::SourceId) {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(
            AppState::open(&ServiceConfig {
                data_dir: dir.path().join("data"),
                auto_reconcile: false,
                content_workers: 1,
                fleet: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let source = state
            .catalog
            .add_source(&NewSource {
                host_id: state.host_id,
                name: "admission-test".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: r"\\server\share".into(),
                aliases: vec![],
            })
            .unwrap();
        state.content_budgets().set(source, 1);
        state
            .catalog
            .enqueue_many(&[NewJob {
                source_id: source,
                object_id: None,
                object_generation: 1,
                stage: JobStage::ContentText,
                priority: Priority::NormalText,
                idempotency_key: "admission-test".into(),
                payload: None,
                estimated_cost: 0,
            }])
            .unwrap();
        (dir, state, source)
    }

    #[test]
    fn a_completed_pause_cannot_be_followed_by_a_stale_claim() {
        let (_dir, state, _source) = state_with_queued_job();

        // Hold the admission gate exactly as the pause endpoint does and let
        // a worker wait behind it. The stopped response is derived before the
        // worker proceeds, and its authoritative recheck then rejects work.
        let admission = state.content_pause.admission_guard();
        let worker = {
            let state = state.clone();
            std::thread::spawn(move || reserve_and_claim(&state, "content-test", 1).unwrap())
        };
        state.content_pause.set_paused_admitted(true).unwrap();
        assert_eq!(content_status(&state).flow, ContentFlow::Stopped);
        drop(admission);

        assert!(worker.join().unwrap().is_none());
        assert_eq!(state.content_budgets().reserved_total(), 0);
    }

    #[test]
    fn a_resumed_backlog_waits_for_scan_ownership_to_end() {
        let (_dir, state, source) = state_with_queued_job();
        state.content_pause.set_paused(true).unwrap();
        state
            .scans
            .lock()
            .insert(source, Arc::new(ScanProgress::new(source)));

        state.content_pause.set_paused(false).unwrap();
        assert!(
            reserve_and_claim(&state, "content-test", 1)
                .unwrap()
                .is_none(),
            "resuming does not bypass an active scan's source ownership"
        );
        assert_eq!(state.content_budgets().reserved_total(), 0);
    }
}
