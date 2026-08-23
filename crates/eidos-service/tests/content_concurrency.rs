//! Per-source content concurrency: capacity is reserved atomically before
//! any job is claimed, so a budget is never exceeded, is always released,
//! and a saturated source never starves the others.

use eidos_catalog::jobs::NewJob;
use eidos_catalog::NewSource;
use eidos_domain::{JobStage, Priority, SourceId, SourceKind};
use eidos_service::content_workers::{reserve_and_claim, spawn_content_workers};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

struct Env {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServiceConfig {
        data_dir: dir.path().join("data"),
        scan_threads: 1,
        auto_reconcile: false,
        content_workers: 1,
        ..Default::default()
    };
    Env {
        state: Arc::new(AppState::open(&cfg).unwrap()),
        _dir: dir,
    }
}

/// A source with no scanned tree; only its identity matters here.
fn add_source(state: &AppState, name: &str) -> SourceId {
    state
        .catalog
        .add_source(&NewSource {
            host_id: state.host_id,
            name: name.into(),
            kind: SourceKind::WindowsGeneric,
            root_path: format!("\\\\fileserver\\share\\{name}"),
            aliases: vec![],
        })
        .unwrap()
}

/// Queue `n` content jobs for `source`. They carry no object, so a worker
/// would complete them immediately; these tests only exercise claiming.
fn queue_jobs(state: &AppState, source: SourceId, n: u32) {
    let jobs: Vec<NewJob> = (0..n)
        .map(|i| NewJob {
            source_id: source,
            object_id: None,
            object_generation: 1,
            stage: JobStage::ContentText,
            priority: Priority::NormalText,
            idempotency_key: format!("content_text:{}:{i}", source.0),
            payload: None,
            estimated_cost: 0,
        })
        .collect();
    assert_eq!(state.catalog.enqueue_many(&jobs).unwrap(), n as usize);
}

/// Several workers start at the same instant against one source whose
/// budget is one. The reservation is taken inside the claiming transaction,
/// so exactly one of them may hold the source at any moment.
#[test]
fn a_budget_of_one_is_never_exceeded_by_racing_workers() {
    const WORKERS: usize = 8;
    const ROUNDS: usize = 12;

    let e = env();
    let sid = add_source(&e.state, "single");
    queue_jobs(&e.state, sid, 400);
    e.state.content_budgets().set(sid, 1);

    // Independent of the budget bookkeeping: what the test itself observes.
    let in_flight = Arc::new(AtomicU32::new(0));
    let observed_peak = Arc::new(AtomicU32::new(0));
    let claimed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(WORKERS));

    std::thread::scope(|scope| {
        for w in 0..WORKERS {
            let state = e.state.clone();
            let (in_flight, observed_peak, claimed, barrier) = (
                in_flight.clone(),
                observed_peak.clone(),
                claimed.clone(),
                barrier.clone(),
            );
            scope.spawn(move || {
                let name = format!("content-{w}");
                barrier.wait();
                for _ in 0..ROUNDS {
                    let Some((reservation, jobs)) =
                        reserve_and_claim(&state, &name, 4).expect("claim")
                    else {
                        continue;
                    };
                    assert_eq!(reservation.source(), sid);
                    assert!(!jobs.is_empty(), "a reservation implies claimed work");
                    claimed.fetch_add(jobs.len(), Ordering::Relaxed);

                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    observed_peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    drop(reservation);
                }
            });
        }
    });

    assert_eq!(
        observed_peak.load(Ordering::SeqCst),
        1,
        "a budget of one was exceeded"
    );
    assert_eq!(
        e.state.content_budgets().peak_reserved(sid),
        1,
        "reservation high-water mark disagrees with the observed peak"
    );
    assert_eq!(
        e.state.content_budgets().reserved(sid),
        0,
        "every reservation was released"
    );
    assert!(
        claimed.load(Ordering::Relaxed) >= WORKERS,
        "workers made no progress: {}",
        claimed.load(Ordering::Relaxed)
    );
}

/// The same race with a budget of three: the pool saturates the budget but
/// never overshoots it.
#[test]
fn a_larger_budget_is_saturated_but_not_exceeded() {
    const WORKERS: usize = 8;

    let e = env();
    let sid = add_source(&e.state, "triple");
    queue_jobs(&e.state, sid, 400);
    e.state.content_budgets().set(sid, 3);

    let in_flight = Arc::new(AtomicU32::new(0));
    let observed_peak = Arc::new(AtomicU32::new(0));
    let barrier = Arc::new(Barrier::new(WORKERS));

    std::thread::scope(|scope| {
        for w in 0..WORKERS {
            let state = e.state.clone();
            let (in_flight, observed_peak, barrier) =
                (in_flight.clone(), observed_peak.clone(), barrier.clone());
            scope.spawn(move || {
                let name = format!("content-{w}");
                barrier.wait();
                for _ in 0..12 {
                    let Some((reservation, _jobs)) =
                        reserve_and_claim(&state, &name, 4).expect("claim")
                    else {
                        continue;
                    };
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    observed_peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    drop(reservation);
                }
            });
        }
    });

    assert!(
        observed_peak.load(Ordering::SeqCst) <= 3,
        "budget of three exceeded: {}",
        observed_peak.load(Ordering::SeqCst)
    );
    assert_eq!(e.state.content_budgets().reserved(sid), 0);
}

/// A source pinned at its budget is skipped rather than blocking the pool:
/// the next eligible source is claimed instead.
#[test]
fn a_saturated_source_does_not_starve_the_others() {
    let e = env();
    let busy = add_source(&e.state, "busy");
    let idle = add_source(&e.state, "idle");
    // `busy` is enqueued first, so it owns the head of the queue.
    queue_jobs(&e.state, busy, 20);
    queue_jobs(&e.state, idle, 20);
    e.state.content_budgets().set(busy, 1);
    e.state.content_budgets().set(idle, 1);

    let (held, jobs) = reserve_and_claim(&e.state, "content-0", 4)
        .unwrap()
        .expect("first claim");
    assert_eq!(held.source(), busy, "highest-priority source claimed first");
    assert_eq!(jobs.len(), 4);

    let (next, jobs) = reserve_and_claim(&e.state, "content-1", 4)
        .unwrap()
        .expect("second worker must skip past the saturated source");
    assert_eq!(next.source(), idle);
    assert!(jobs.iter().all(|j| j.source_id == idle));

    // With both budgets taken there is nothing left to admit, and nothing
    // extra is reserved by the attempt.
    assert!(reserve_and_claim(&e.state, "content-2", 4)
        .unwrap()
        .is_none());
    assert_eq!(e.state.content_budgets().reserved(busy), 1);
    assert_eq!(e.state.content_budgets().reserved(idle), 1);

    drop(held);
    let (again, _) = reserve_and_claim(&e.state, "content-2", 4)
        .unwrap()
        .expect("capacity freed by the drop");
    assert_eq!(again.source(), busy);
    drop((next, again));
    assert_eq!(e.state.content_budgets().reserved(busy), 0);
    assert_eq!(e.state.content_budgets().reserved(idle), 0);
}

/// Persisted budgets are installed before the first worker can claim.
/// Otherwise a source configured below the default would be oversubscribed
/// for the seconds between startup and the coordinator's first refresh.
#[test]
fn startup_loads_budgets_before_any_worker_claims() {
    let e = env();
    let sid = add_source(&e.state, "hdd");
    e.state.catalog.set_content_policy(sid, true, 1).unwrap();
    queue_jobs(&e.state, sid, 40);
    assert_eq!(
        e.state.content_budgets().budget(sid),
        eidos_service::source_budget::DEFAULT_BUDGET,
        "nothing is known about the source before startup"
    );
    // Extraction paused, so the coordinator's periodic refresh is skipped:
    // only startup can have installed the budget.
    e.state.content_enabled.store(false, Ordering::Relaxed);

    spawn_content_workers(&e.state, 4);
    assert_eq!(
        e.state.content_budgets().budget(sid),
        1,
        "workers must not start against the default budget"
    );
    e.state.request_shutdown();
}

/// If the startup policy load fails the pool still starts, but it must not
/// fall back to the default budget: a source whose policy has never been
/// read admits nothing until a refresh gets through.
#[test]
fn an_unread_policy_admits_no_work_until_a_refresh_lands() {
    let e = env();
    let sid = add_source(&e.state, "unknown");
    e.state.catalog.set_content_policy(sid, true, 1).unwrap();
    queue_jobs(&e.state, sid, 40);

    // Budgets never loaded (as after a failed startup refresh).
    assert!(
        reserve_and_claim(&e.state, "content-0", 4)
            .unwrap()
            .is_none(),
        "an unread policy must not admit work at the default budget"
    );
    assert_eq!(e.state.content_budgets().reserved(sid), 0);

    // The coordinator's next refresh unblocks the pool at the real budget.
    eidos_service::content_workers::refresh_budgets(&e.state).unwrap();
    let (held, jobs) = reserve_and_claim(&e.state, "content-0", 4)
        .unwrap()
        .expect("claim once the policy is known");
    assert_eq!(jobs.len(), 4);
    assert!(
        reserve_and_claim(&e.state, "content-1", 4)
            .unwrap()
            .is_none(),
        "still bounded by the loaded budget of one"
    );
    drop(held);
}

/// Nothing stays reserved when a claim finds no work, when the caller drops
/// the batch after an error, or when a worker unwinds mid-batch.
#[test]
fn reservations_are_released_on_empty_claim_error_and_panic() {
    let e = env();
    let sid = add_source(&e.state, "fixture");
    e.state.content_budgets().set(sid, 1);

    // Empty queue: no candidate source, so nothing is ever reserved.
    assert!(reserve_and_claim(&e.state, "content-0", 4)
        .unwrap()
        .is_none());
    assert_eq!(e.state.content_budgets().reserved(sid), 0);

    // Error path: the caller drops the claim instead of processing it.
    queue_jobs(&e.state, sid, 16);
    let failed: Result<(), &str> = {
        let (_reservation, jobs) = reserve_and_claim(&e.state, "content-0", 4)
            .unwrap()
            .expect("claim");
        assert_eq!(jobs.len(), 4);
        Err("bookkeeping blew up")
    };
    assert!(failed.is_err());
    assert_eq!(
        e.state.content_budgets().reserved(sid),
        0,
        "an abandoned batch must release its capacity"
    );

    // Panic path: the guard is released while unwinding.
    let state = e.state.clone();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let (_reservation, _jobs) = reserve_and_claim(&state, "content-1", 4)
            .unwrap()
            .expect("claim");
        panic!("extraction blew up");
    }));
    std::panic::set_hook(hook);
    assert!(out.is_err());
    assert_eq!(e.state.content_budgets().reserved(sid), 0);

    // Capacity is usable again afterwards.
    assert!(reserve_and_claim(&e.state, "content-2", 4)
        .unwrap()
        .is_some());
    assert_eq!(e.state.content_budgets().reserved(sid), 0);
    assert_eq!(e.state.content_budgets().peak_reserved(sid), 1);
}
