//! Single-flight OS probes. A stuck syscall keeps its one dedicated thread;
//! HTTP timeouts never release capacity or launch replacement probes.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct BackgroundProbe<T> {
    state: Mutex<ProbeState<T>>,
    changed: tokio::sync::Notify,
}

struct ProbeState<T> {
    result: Option<(Instant, Result<T, String>)>,
    running: bool,
}

impl<T: Clone + Send + 'static> Default for BackgroundProbe<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(ProbeState {
                result: None,
                running: false,
            }),
            changed: tokio::sync::Notify::new(),
        }
    }
}

impl<T: Clone + Send + 'static> BackgroundProbe<T> {
    pub fn seeded(result: Result<T, String>) -> Self {
        Self::seeded_at(result, Duration::ZERO)
    }

    /// Seed a result that already carries `age`, so age-sensitive callers can
    /// be exercised without sleeping.
    pub fn seeded_at(result: Result<T, String>, age: Duration) -> Self {
        let now = Instant::now();
        Self {
            state: Mutex::new(ProbeState {
                result: Some((now.checked_sub(age).unwrap_or(now), result)),
                running: false,
            }),
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> Option<(Duration, Result<T, String>)> {
        self.state
            .lock()
            .result
            .as_ref()
            .map(|(at, result)| (at.elapsed(), result.clone()))
    }

    pub fn refresh(
        self: &Arc<Self>,
        ttl: Duration,
        probe: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) {
        let mut state = self.state.lock();
        if state.running
            || state
                .result
                .as_ref()
                .is_some_and(|(at, _)| at.elapsed() < ttl)
        {
            return;
        }
        state.running = true;
        let this = self.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("os-probe".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(probe))
                    .unwrap_or_else(|_| Err("OS probe panicked".into()));
                let mut state = this.state.lock();
                state.result = Some((Instant::now(), result));
                state.running = false;
                drop(state);
                this.changed.notify_waiters();
            })
        {
            state.running = false;
            // A transient spawn failure must not blank a warm cache or
            // restart its TTL: that would turn one failed thread into a
            // whole TTL of errors for every caller.
            if state.result.is_none() {
                state.result = Some((Instant::now(), Err(error.to_string())));
            }
            self.changed.notify_waiters();
        }
    }

    /// Wait only for a cold cache. A warm caller gets the cached answer while
    /// any refresh continues separately (including a stuck OS call).
    pub async fn cached(&self, deadline: Duration) -> Result<T, String> {
        self.cached_within(None, deadline).await.unwrap_or_else(|| {
            Err("OS probe is still running; retry shortly or enter a path manually".to_string())
        })
    }

    /// Wait, bounded by `deadline`, for a cached result no older than `max_age`
    /// (any age when `None`). `None` means nothing usable arrived in time: the
    /// running probe keeps its thread and no replacement is ever started. An
    /// age bound lets a one-shot caller receive the refresh it just triggered
    /// instead of an arbitrarily old sample.
    pub async fn cached_within(
        &self,
        max_age: Option<Duration>,
        deadline: Duration,
    ) -> Option<Result<T, String>> {
        let wait = async {
            loop {
                let changed = self.changed.notified();
                // Register before inspecting state to avoid a lost wake-up.
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some((age, result)) = self.snapshot() {
                    if max_age.is_none_or(|max| age <= max) {
                        return result;
                    }
                }
                changed.await;
            }
        };
        tokio::time::timeout(deadline, wait).await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    #[tokio::test]
    async fn timeout_and_retries_keep_one_probe_until_it_really_finishes() {
        let probe = Arc::new(BackgroundProbe::<u32>::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, blocked) = mpsc::channel();
        let count = calls.clone();
        probe.refresh(Duration::ZERO, move || {
            count.fetch_add(1, Ordering::Relaxed);
            blocked.recv().unwrap();
            Ok(7)
        });
        assert!(probe.cached(Duration::from_millis(20)).await.is_err());
        for _ in 0..50 {
            probe.refresh(Duration::ZERO, || panic!("duplicate probe"));
        }
        release.send(()).unwrap();
        assert_eq!(probe.cached(Duration::from_secs(2)).await.unwrap(), 7);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        probe.refresh(Duration::from_secs(60), || panic!("cache bypass"));
        assert_eq!(probe.cached(Duration::ZERO).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn warm_cache_is_available_during_a_slow_refresh() {
        let probe = Arc::new(BackgroundProbe::seeded(Ok(3)));
        let (release, blocked) = mpsc::channel();
        probe.refresh(Duration::ZERO, move || {
            blocked.recv().unwrap();
            Ok(4)
        });
        assert_eq!(probe.cached(Duration::from_millis(20)).await.unwrap(), 3);
        release.send(()).unwrap();
    }

    #[tokio::test]
    async fn an_age_bound_waits_for_a_refresh_without_starting_a_second_probe() {
        let probe = Arc::new(BackgroundProbe::seeded_at(Ok(1), Duration::from_secs(60)));
        let usable = Some(Duration::from_secs(30));
        // Nothing is refreshing yet, so only the deadline can end this wait.
        assert!(probe
            .cached_within(usable, Duration::from_millis(20))
            .await
            .is_none());
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, blocked) = mpsc::channel();
        let count = calls.clone();
        probe.refresh(Duration::ZERO, move || {
            count.fetch_add(1, Ordering::Relaxed);
            blocked.recv().unwrap();
            Ok(2)
        });
        // A caller that accepts any age still gets the old value immediately,
        // and neither caller launches a replacement probe.
        assert_eq!(probe.cached(Duration::ZERO).await.unwrap(), 1);
        assert!(probe
            .cached_within(usable, Duration::from_millis(20))
            .await
            .is_none());
        release.send(()).unwrap();
        assert_eq!(
            probe
                .cached_within(usable, Duration::from_secs(2))
                .await
                .unwrap()
                .unwrap(),
            2
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
