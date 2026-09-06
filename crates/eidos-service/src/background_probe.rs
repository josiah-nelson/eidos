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
        Self {
            state: Mutex::new(ProbeState {
                result: Some((Instant::now(), result)),
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
            state.result = Some((Instant::now(), Err(error.to_string())));
            self.changed.notify_waiters();
        }
    }

    /// Wait only for a cold cache. A warm caller gets the cached answer while
    /// any refresh continues separately (including a stuck OS call).
    pub async fn cached(&self, deadline: Duration) -> Result<T, String> {
        let wait = async {
            loop {
                let changed = self.changed.notified();
                // Register before inspecting state to avoid a lost wake-up.
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some((_, result)) = self.snapshot() {
                    return result;
                }
                changed.await;
            }
        };
        tokio::time::timeout(deadline, wait).await.map_err(|_| {
            "OS probe is still running; retry shortly or enter a path manually".to_string()
        })?
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
}
