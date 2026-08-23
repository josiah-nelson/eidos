//! Admission control for expensive HTTP operations.
//!
//! Search, browse, archive listing and count endpoints run blocking
//! SQLite/Tantivy work on the blocking pool. Without a bound, a burst of
//! broad regex searches or large counts fills that pool, starves the
//! lightweight health/status endpoints behind it, and turns overload into
//! unbounded latency instead of a clear answer.
//!
//! This module puts one bounded gate in front of those operations:
//!
//! - a semaphore with `concurrency` permits, taken only by expensive
//!   operations (health, activity and index status never enter it);
//! - a bounded queue: at most `queue_depth` requests may wait for a permit,
//!   for at most `queue_wait`. Anything beyond that is shed immediately with
//!   a structured busy error and a `Retry-After` hint;
//! - a per-operation timeout that ends the *response*, not the work.
//!
//! ## Permits outlive timed-out responses
//!
//! The blocking work cannot be cancelled mid-call: a `rusqlite` statement or
//! a Tantivy collector runs to completion on its thread, and killing it is
//! not an option we have. So the permit is owned by the blocking closure and
//! released by its `Drop` — on the blocking thread, when the work actually
//! finishes — never by the awaiting HTTP task. A timeout therefore returns
//! `504` to the client while the query keeps running and keeps holding its
//! permit; the catalog and the index see an ordinary, complete operation.
//!
//! The accounting reflects that: `in_flight` counts blocking tasks that still
//! hold a permit, so it stays elevated after a timeout, and `detached` counts
//! how many of those no longer have a client waiting — because the deadline
//! passed, or because the client disconnected and axum dropped the handler.
//! Both return to zero once the abandoned work drains, which is what makes
//! "no permit leaks" testable.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use ts_rs::TS;

/// Tunables for the expensive-operation gate.
#[derive(Debug, Clone)]
pub struct AdmissionConfig {
    /// Expensive operations allowed to run at once.
    pub concurrency: usize,
    /// Requests allowed to wait for a permit before shedding load.
    pub queue_depth: usize,
    /// How long a queued request waits before it is shed.
    pub queue_wait: Duration,
    /// Response deadline for search.
    pub search_timeout: Duration,
    /// Response deadline for the other expensive operations.
    pub operation_timeout: Duration,
    /// Maximum JSON request body accepted by the API.
    pub max_body_bytes: usize,
}

impl Default for AdmissionConfig {
    /// Loopback defaults: a single interactive UI plus a CLI, on a machine
    /// that is also scanning and extracting content. Four concurrent heavy
    /// queries keep the blocking pool available for background work.
    fn default() -> Self {
        Self {
            concurrency: 4,
            queue_depth: 32,
            queue_wait: Duration::from_secs(5),
            search_timeout: Duration::from_secs(30),
            operation_timeout: Duration::from_secs(60),
            max_body_bytes: 1 << 20,
        }
    }
}

/// Operation classes that pass through the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expensive {
    /// Catalog/content query execution.
    Search,
    /// One page of a streaming search export.
    Export,
    /// Directory listing and object detail.
    Browse,
    /// Archive manifest listing.
    Archive,
    /// Aggregate counts over a subtree or source.
    Counts,
    /// Bulk catalog maintenance triggered from the API.
    Maintenance,
}

impl Expensive {
    pub fn label(self) -> &'static str {
        match self {
            Expensive::Search => "search",
            Expensive::Export => "export",
            Expensive::Browse => "browse",
            Expensive::Archive => "archive",
            Expensive::Counts => "counts",
            Expensive::Maintenance => "maintenance",
        }
    }
}

/// Why an expensive operation did not produce a result.
#[derive(Debug)]
pub enum AdmissionError {
    /// Shed before running: the queue was full, the wait expired, or the
    /// service is shutting down.
    Busy {
        op: &'static str,
        message: String,
        retry_after_s: u64,
    },
    /// Admitted and still running, but the response deadline passed.
    Timeout { op: &'static str, after: Duration },
    /// The blocking task panicked or was cancelled by runtime shutdown.
    Failed(String),
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionError::Busy { message, .. } => f.write_str(message),
            AdmissionError::Timeout { op, after } => {
                write!(
                    f,
                    "{op} exceeded its {:.0}s time limit",
                    after.as_secs_f64()
                )
            }
            AdmissionError::Failed(m) => write!(f, "operation failed: {m}"),
        }
    }
}

const RUNNING: u8 = 0;
const FINISHED: u8 = 1;
const DETACHED: u8 = 2;

#[derive(Default, Debug)]
struct Counters {
    in_flight: AtomicU64,
    queued: AtomicU64,
    detached: AtomicU64,
    admitted: AtomicU64,
    completed: AtomicU64,
    rejected_busy: AtomicU64,
    timed_out: AtomicU64,
}

/// Releases the permit and updates the gauges on the blocking thread, when
/// the work is genuinely over (including on panic, during unwind).
struct Work {
    counters: Arc<Counters>,
    state: Arc<AtomicU8>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Work {
    fn drop(&mut self) {
        if self.state.swap(FINISHED, Ordering::AcqRel) == DETACHED {
            self.counters.detached.fetch_sub(1, Ordering::Relaxed);
        }
        self.counters.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.counters.completed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Marks admitted work as detached when the awaiting request goes away —
/// whether it timed out or the client disconnected — unless the work already
/// finished. Lives on the async side; the permit is unaffected.
struct Abandoned {
    counters: Arc<Counters>,
    state: Arc<AtomicU8>,
}

impl Drop for Abandoned {
    fn drop(&mut self) {
        // Count first, publish second. The completing side only decrements
        // after it reads `DETACHED`, which the release below orders after
        // this increment, so the gauge can never go negative when the work
        // finishes in the middle of this.
        self.counters.detached.fetch_add(1, Ordering::Relaxed);
        if self
            .state
            .compare_exchange(RUNNING, DETACHED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // The work finished first and will never see `DETACHED`; it was
            // not abandoned after all.
            self.counters.detached.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Decrements the queue gauge however the wait ends.
struct Waiting(Arc<Counters>);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.queued.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Test hook: invoked on the blocking thread just before the admitted work
/// runs, with the operation label. Always unset in the service.
pub type BlockingHook = Arc<dyn Fn(&'static str) + Send + Sync>;

pub struct Admission {
    config: AdmissionConfig,
    permits: Arc<Semaphore>,
    counters: Arc<Counters>,
    hook: parking_lot::Mutex<Option<BlockingHook>>,
}

impl Admission {
    pub fn new(mut config: AdmissionConfig) -> Self {
        // A gate of zero would close the API entirely; report what is enforced.
        config.concurrency = config.concurrency.max(1);
        Self {
            permits: Arc::new(Semaphore::new(config.concurrency)),
            config,
            counters: Arc::new(Counters::default()),
            hook: parking_lot::Mutex::new(None),
        }
    }

    pub fn config(&self) -> &AdmissionConfig {
        &self.config
    }

    /// Install the test hook (see [`BlockingHook`]).
    pub fn set_blocking_hook(&self, hook: Option<BlockingHook>) {
        *self.hook.lock() = hook;
    }

    /// Stop admitting expensive work. Queued waiters are shed at once so a
    /// graceful shutdown drains only the operations already running.
    pub fn close(&self) {
        self.permits.close();
    }

    fn timeout_for(&self, op: Expensive) -> Duration {
        match op {
            Expensive::Search | Expensive::Export => self.config.search_timeout,
            _ => self.config.operation_timeout,
        }
    }

    fn busy(&self, op: Expensive, message: impl Into<String>) -> AdmissionError {
        self.counters.rejected_busy.fetch_add(1, Ordering::Relaxed);
        let message = message.into();
        tracing::warn!(op = op.label(), reason = %message, "shedding expensive request");
        AdmissionError::Busy {
            op: op.label(),
            message,
            // Deliberately coarse: the wait bound is the only honest estimate
            // of when a permit might be free, and `Retry-After` is seconds.
            retry_after_s: self.config.queue_wait.as_secs().clamp(1, 60),
        }
    }

    /// Run `f` on the blocking pool under the gate.
    ///
    /// Returns [`AdmissionError::Timeout`] when the deadline passes; `f` keeps
    /// running to completion and holds its permit until it does.
    pub async fn run<T, F>(&self, op: Expensive, f: F) -> Result<T, AdmissionError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.run_inner(op, true, f).await
    }

    /// Run one export page only when it can start immediately.
    ///
    /// Export pages never join the shared queue. If interactive work is
    /// already waiting, the page also yields a permit it happened to acquire;
    /// this prevents a fast cursor walk from repeatedly getting ahead of a
    /// queued search when the gate has only one slot.
    pub(crate) async fn run_export<T, F>(&self, f: F) -> Result<T, AdmissionError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.run_inner(Expensive::Export, false, f).await
    }

    async fn run_inner<T, F>(
        &self,
        op: Expensive,
        allow_queue: bool,
        f: F,
    ) -> Result<T, AdmissionError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(TryAcquireError::Closed) => return Err(self.busy(op, "service is shutting down")),
            Err(TryAcquireError::NoPermits) if !allow_queue => {
                return Err(self.busy(op, "no free slot; export pages do not queue"));
            }
            Err(TryAcquireError::NoPermits) => {
                let waiting = self.counters.queued.fetch_add(1, Ordering::Relaxed) + 1;
                let _waiting = Waiting(self.counters.clone());
                if waiting > self.config.queue_depth as u64 {
                    return Err(self.busy(
                        op,
                        format!(
                            "too many queued requests ({} waiting, {} running)",
                            waiting - 1,
                            self.config.concurrency
                        ),
                    ));
                }
                match tokio::time::timeout(
                    self.config.queue_wait,
                    self.permits.clone().acquire_owned(),
                )
                .await
                {
                    Ok(Ok(p)) => p,
                    Ok(Err(_closed)) => {
                        return Err(self.busy(op, "service is shutting down"));
                    }
                    Err(_elapsed) => {
                        return Err(self.busy(
                            op,
                            format!(
                                "waited {:.0}s for a free slot",
                                self.config.queue_wait.as_secs_f64()
                            ),
                        ));
                    }
                }
            }
        };
        if !allow_queue && self.counters.queued.load(Ordering::Acquire) > 0 {
            drop(permit);
            return Err(self.busy(op, "interactive work is waiting; export page yielded"));
        }

        self.counters.in_flight.fetch_add(1, Ordering::Relaxed);
        self.counters.admitted.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(AtomicU8::new(RUNNING));
        let work = Work {
            counters: self.counters.clone(),
            state: state.clone(),
            _permit: permit,
        };
        let hook = self.hook.lock().clone();
        let handle = tokio::task::spawn_blocking(move || {
            // Dropped last, on this thread: the permit is held for exactly as
            // long as the blocking work runs.
            let _work = work;
            if let Some(hook) = hook {
                hook(op.label());
            }
            f()
        });
        // Every way of losing the client — deadline, or this future being
        // dropped because the connection went away — leaves the blocking task
        // running. Marking it here catches all of them; on the success paths
        // the work already reported itself finished, so this is a no-op.
        let _abandoned = Abandoned {
            counters: self.counters.clone(),
            state: state.clone(),
        };

        let deadline = self.timeout_for(op);
        match tokio::time::timeout(deadline, handle).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(join)) => Err(AdmissionError::Failed(join.to_string())),
            Err(_elapsed) => {
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    op = op.label(),
                    timeout_ms = deadline.as_millis() as u64,
                    "expensive request timed out; its blocking work continues"
                );
                Err(AdmissionError::Timeout {
                    op: op.label(),
                    after: deadline,
                })
            }
        }
    }

    pub fn view(&self) -> AdmissionView {
        let c = &self.counters;
        AdmissionView {
            limit: self.config.concurrency,
            queue_depth: self.config.queue_depth,
            in_flight: c.in_flight.load(Ordering::Relaxed),
            queued: c.queued.load(Ordering::Relaxed),
            detached: c.detached.load(Ordering::Relaxed),
            admitted: c.admitted.load(Ordering::Relaxed),
            completed: c.completed.load(Ordering::Relaxed),
            rejected_busy: c.rejected_busy.load(Ordering::Relaxed),
            timed_out: c.timed_out.load(Ordering::Relaxed),
            queue_wait_ms: self.config.queue_wait.as_millis() as u64,
            search_timeout_ms: self.config.search_timeout.as_millis() as u64,
            operation_timeout_ms: self.config.operation_timeout.as_millis() as u64,
            max_body_bytes: self.config.max_body_bytes,
        }
    }
}

/// Additive admission block reported by `GET /api/index` and `/api/activity`.
#[derive(Debug, Clone, serde::Serialize, TS)]
pub struct AdmissionView {
    /// Expensive operations admitted concurrently.
    pub limit: usize,
    pub queue_depth: usize,
    /// Blocking tasks holding a permit right now (includes work whose client
    /// already gave up).
    pub in_flight: u64,
    /// Requests waiting for a permit right now.
    pub queued: u64,
    /// In-flight work with no client left waiting for it (timed out, or the
    /// connection went away).
    pub detached: u64,
    pub admitted: u64,
    pub completed: u64,
    pub rejected_busy: u64,
    pub timed_out: u64,
    pub queue_wait_ms: u64,
    pub search_timeout_ms: u64,
    pub operation_timeout_ms: u64,
    pub max_body_bytes: usize,
}
