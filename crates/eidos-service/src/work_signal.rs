//! Content-work wakeups are independent from catalog writer completions: an
//! empty claim completes a writer turn and must not wake another empty claim.

use parking_lot::{Condvar, Mutex};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[derive(Debug, Default)]
pub(crate) struct WorkSignal {
    epoch: Mutex<u64>,
    changed: Condvar,
    waiting: AtomicUsize,
}

impl WorkSignal {
    pub fn epoch(&self) -> u64 {
        *self.epoch.lock()
    }

    pub fn notify_all(&self) {
        let mut epoch = self.epoch.lock();
        *epoch = epoch.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Capture the epoch BEFORE checking shutdown/admission/work availability.
    /// A notification between that check and this wait cannot be lost.
    pub fn wait(&self, observed: u64, timeout: Duration) {
        let mut epoch = self.epoch.lock();
        if *epoch != observed {
            return;
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);
        self.changed.wait_for(&mut epoch, timeout);
        self.waiting.fetch_sub(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Instant};

    #[test]
    fn a_notification_between_check_and_park_is_not_lost() {
        let signal = WorkSignal::default();
        let observed = signal.epoch();
        signal.notify_all();
        let start = Instant::now();
        signal.wait(observed, Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(signal.waiting(), 0);
    }

    #[test]
    fn one_control_notification_wakes_all_parked_workers() {
        let signal = Arc::new(WorkSignal::default());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let signal = signal.clone();
                std::thread::spawn(move || signal.wait(signal.epoch(), Duration::from_secs(10)))
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(2);
        while signal.waiting() < 8 {
            assert!(Instant::now() < deadline, "workers did not park");
            std::thread::yield_now();
        }
        signal.notify_all();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(signal.waiting(), 0);
    }

    #[test]
    fn a_quiet_wait_expires_without_creating_a_new_notification() {
        let signal = WorkSignal::default();
        let observed = signal.epoch();
        let start = Instant::now();
        signal.wait(observed, Duration::from_millis(50));
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(signal.epoch(), observed);
        assert_eq!(signal.waiting(), 0);
    }
}
