//! Content-work wakeups are independent from catalog writer completions: an
//! empty claim completes a writer turn and must not wake another empty claim.

use parking_lot::{Condvar, Mutex};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

/// One epoch, two wait sets.
///
/// The epoch is what makes a wakeup impossible to lose: a caller reads it
/// before it checks its own state, and parks only if nothing has advanced it
/// since. The two wait sets are what keep a single-worker wakeup useful. A
/// worker parked above the pool size cannot claim anything however much work
/// is due, so spending a work hint on it would leave the due job sitting
/// there until the next hint — for a pool shrunk from sixty-four that is a
/// long wait built out of wasted wakeups. Work hints therefore reach only the
/// waiters that can act on them; control transitions reach everyone.
#[derive(Debug, Default)]
pub(crate) struct WorkSignal {
    epoch: Mutex<u64>,
    /// Workers inside the pool size: woken by control changes and work hints.
    claimable: Condvar,
    /// Workers parked above the pool size: only a control change can make
    /// them eligible again, so no work hint is ever spent here.
    surplus: Condvar,
    waiting: AtomicUsize,
}

impl WorkSignal {
    pub fn epoch(&self) -> u64 {
        *self.epoch.lock()
    }

    /// Wake every parked worker, in both wait sets. Use for control
    /// transitions — pause/resume, resize, shutdown, limit changes — where
    /// every worker has to re-evaluate its own state before it can decide
    /// anything, including whether it is still surplus.
    pub fn notify_all(&self) {
        let mut epoch = self.epoch.lock();
        *epoch = epoch.wrapping_add(1);
        self.claimable.notify_all();
        self.surplus.notify_all();
    }

    /// Wake one worker that is allowed to claim. Use for "there may be due
    /// work" hints, which admission is still free to refuse. A worker that
    /// does claim hands the baton on, so a genuinely admittable backlog fills
    /// the pool in a chain; a backlog admission refuses costs one empty claim
    /// per hint instead of one per worker. Both calls advance the epoch: a
    /// worker between its work check and parking must never park on stale
    /// state.
    pub fn notify_one(&self) {
        let mut epoch = self.epoch.lock();
        *epoch = epoch.wrapping_add(1);
        self.claimable.notify_one();
    }

    /// Park a worker that may claim. Capture the epoch BEFORE checking
    /// shutdown/admission/work availability: a notification between that
    /// check and this wait cannot be lost.
    pub fn wait(&self, observed: u64, timeout: Duration) {
        self.park(&self.claimable, observed, timeout);
    }

    /// Park a worker that is above the pool size. Only a control transition
    /// can change that, so this waiter is deliberately deaf to work hints.
    /// It still returns on any epoch change it has not observed yet, so a
    /// resize racing the capture above cannot strand it.
    pub fn wait_for_control(&self, observed: u64, timeout: Duration) {
        self.park(&self.surplus, observed, timeout);
    }

    fn park(&self, set: &Condvar, observed: u64, timeout: Duration) {
        let mut epoch = self.epoch.lock();
        if *epoch != observed {
            return;
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);
        set.wait_for(&mut epoch, timeout);
        self.waiting.fetch_sub(1, Ordering::Relaxed);
    }

    /// Parked workers across both wait sets.
    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread::JoinHandle, time::Instant};

    /// Park `count` threads in one of the two wait sets and return once they
    /// are all actually blocked, so a wakeup cannot be counted before it.
    fn park_all(signal: &Arc<WorkSignal>, count: usize, surplus: bool) -> Vec<JoinHandle<()>> {
        let before = signal.waiting();
        let threads: Vec<_> = (0..count)
            .map(|_| {
                let signal = signal.clone();
                std::thread::spawn(move || {
                    let observed = signal.epoch();
                    if surplus {
                        signal.wait_for_control(observed, Duration::from_secs(10));
                    } else {
                        signal.wait(observed, Duration::from_secs(10));
                    }
                })
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(2);
        while signal.waiting() < before + count {
            assert!(Instant::now() < deadline, "workers did not park");
            std::thread::yield_now();
        }
        threads
    }

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
    fn a_surplus_waiter_also_keeps_a_control_change_it_raced() {
        let signal = WorkSignal::default();
        let observed = signal.epoch();
        signal.notify_all();
        let start = Instant::now();
        signal.wait_for_control(observed, Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(signal.waiting(), 0);
    }

    #[test]
    fn one_control_notification_wakes_all_parked_workers() {
        let signal = Arc::new(WorkSignal::default());
        let mut threads = park_all(&signal, 5, false);
        threads.extend(park_all(&signal, 3, true));
        assert_eq!(signal.waiting(), 8);
        signal.notify_all();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(signal.waiting(), 0);
    }

    #[test]
    fn a_work_hint_wakes_exactly_one_parked_worker() {
        let signal = Arc::new(WorkSignal::default());
        let threads = park_all(&signal, 8, false);
        signal.notify_one();
        let deadline = Instant::now() + Duration::from_secs(2);
        while signal.waiting() > 7 {
            assert!(Instant::now() < deadline, "the hint woke nobody");
            std::thread::yield_now();
        }
        // The other seven stay parked rather than all pile into a claim.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(signal.waiting(), 7);
        signal.notify_all();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(signal.waiting(), 0);
    }

    #[test]
    fn a_work_hint_is_never_spent_on_a_worker_parked_above_the_pool_size() {
        let signal = Arc::new(WorkSignal::default());
        // Seven surplus waiters queued ahead of the one worker that can act on
        // a hint. A shared wait set hands the baton to a thread that cannot
        // claim, and the due work then waits for the next hint.
        let mut threads = park_all(&signal, 7, true);
        threads.extend(park_all(&signal, 1, false));
        signal.notify_one();
        let deadline = Instant::now() + Duration::from_secs(2);
        while signal.waiting() > 7 {
            assert!(Instant::now() < deadline, "the hint woke nobody");
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(signal.waiting(), 7, "only the claimable worker may wake");
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
