//! L2: the ETW access lane, run in randomized windows so the workload
//! cannot align with the observation and so control periods exist for the
//! observer-effect comparison. Off by default; `minutes_per_hour = 60`
//! makes it continuous for dedicated hosts.

use crate::access::{process_class, AccessAggregator};
use crate::daemon::Shared;
use crate::etw::{self, Session, TraceEvent};
use eidos_observe::{EtwState, ObservationRecord, ProcessClass};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(600);

pub fn start(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("etw-scheduler".into())
        .spawn(move || scheduler(shared))
        .expect("spawn etw scheduler")
}

struct Plan {
    hour_started: Instant,
    window_start: Instant,
    window_len: Duration,
}

fn plan_hour(hour_started: Instant, minutes_per_hour: u32) -> Plan {
    let minutes = minutes_per_hour.clamp(1, 60);
    let slack_minutes = 60 - minutes;
    let offset_minutes = if slack_minutes == 0 {
        0
    } else {
        let mut bytes = [0u8; 4];
        let _ = getrandom::fill(&mut bytes);
        u32::from_le_bytes(bytes) % (slack_minutes + 1)
    };
    Plan {
        hour_started,
        window_start: hour_started + Duration::from_secs(offset_minutes as u64 * 60),
        window_len: Duration::from_secs(minutes as u64 * 60),
    }
}

fn scheduler(shared: Arc<Shared>) {
    etw::stop_stale_session();
    let mut plan: Option<Plan> = None;
    let mut blocked_until: Option<Instant> = None;
    while !shared.is_shutting_down() {
        let (enabled, minutes) = {
            let config = shared.config.lock().unwrap();
            (config.lanes.etw.enabled, config.lanes.etw.minutes_per_hour)
        };
        if !enabled {
            plan = None;
            set_view(&shared, |v| {
                v.state = "off".into();
                v.window_open = false;
                v.next_window_s = None;
            });
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        if let Some(until) = blocked_until {
            if Instant::now() < until {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            blocked_until = None;
        }
        let now = Instant::now();
        let current = match plan.take() {
            Some(p) if now < p.hour_started + Duration::from_secs(3600) => p,
            Some(p) => plan_hour(p.hour_started + Duration::from_secs(3600), minutes),
            None => plan_hour(now, minutes),
        };
        if now < current.window_start {
            let wait = current.window_start - now;
            set_view(&shared, |v| {
                v.state = "scheduled".into();
                v.window_open = false;
                v.next_window_s = Some(wait.as_secs());
            });
            plan = Some(current);
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let window_end = current.window_start + current.window_len;
        if now >= window_end {
            plan = Some(current);
            continue;
        }
        match run_window(&shared, window_end) {
            Ok(()) => {}
            Err(code) => {
                let state = match code {
                    5 => EtwState::AccessDenied,
                    183 | 1450 => EtwState::SessionConflict,
                    _ => EtwState::InternalError,
                };
                tracing::warn!(code, state = ?state, "ETW session unavailable");
                if let Some(windows) = shared.capabilities.lock().unwrap().windows.as_mut() {
                    windows.etw = state;
                }
                set_view(&shared, |v| {
                    v.state = format!("{state:?}").to_ascii_lowercase();
                    v.window_open = false;
                });
                blocked_until = Some(Instant::now() + RETRY_AFTER_FAILURE);
            }
        }
        plan = Some(current);
    }
    etw::stop_stale_session();
}

fn set_view(shared: &Shared, f: impl FnOnce(&mut crate::protocol::EtwView)) {
    f(&mut shared.etw.lock().unwrap());
}

fn run_window(shared: &Arc<Shared>, window_end: Instant) -> Result<(), u32> {
    let session = Session::start()?;
    let (sender, receiver) = mpsc::channel::<TraceEvent>();
    let events = Arc::new(AtomicU64::new(0));
    let consumer = {
        let events = events.clone();
        std::thread::Builder::new()
            .name("etw-consumer".into())
            .spawn(move || etw::consume(sender, events))
            .map_err(|_| 8u32)?
    };
    let mut aggregator = AccessAggregator::new();
    let own_pid = std::process::id();
    let seeded = shared.with_key(|key| {
        for (pid, image) in running_processes() {
            let class = if pid == own_pid {
                ProcessClass::Indexer
            } else {
                process_class(&image, key)
            };
            aggregator.process_seen(pid, class, false);
        }
    });
    if seeded.is_none() {
        tracing::warn!("ETW window without a study key: unclassified processes stay 'other'");
    }
    if let Some(windows) = shared.capabilities.lock().unwrap().windows.as_mut() {
        windows.etw = EtwState::Available;
    }
    shared.etw_window_open.store(true, Ordering::Release);
    set_view(shared, |v| {
        v.state = "tracing".into();
        v.window_open = true;
        v.next_window_s = None;
    });
    tracing::info!(
        seconds = (window_end - Instant::now()).as_secs(),
        "ETW window opened"
    );

    let mut last_flush = Instant::now();
    let mut received = 0u64;
    loop {
        let stop = shared.is_shutting_down()
            || !shared.lane_enabled(|c| c.lanes.etw.enabled)
            || Instant::now() >= window_end;
        if stop {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => {
                received += 1;
                handle(shared, &mut aggregator, event);
                // Drain what is queued without waiting again.
                while let Ok(event) = receiver.try_recv() {
                    received += 1;
                    handle(shared, &mut aggregator, event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let rate_s = shared.config.lock().unwrap().intervals.rate_s.max(10) as u64;
        if last_flush.elapsed() >= Duration::from_secs(rate_s) {
            flush(shared, &mut aggregator, last_flush.elapsed());
            last_flush = Instant::now();
            set_view(shared, |v| v.events = received);
        }
    }
    let lost = session.stop();
    let _ = consumer.join();
    while let Ok(event) = receiver.try_recv() {
        received += 1;
        handle(shared, &mut aggregator, event);
    }
    flush(shared, &mut aggregator, last_flush.elapsed());
    shared.etw_window_open.store(false, Ordering::Release);
    if lost > 0 {
        shared.drops.lock().unwrap().kernel += lost as u64;
    }
    set_view(shared, |v| {
        v.state = "scheduled".into();
        v.window_open = false;
        v.events = received;
        v.lost_events += lost as u64;
    });
    tracing::info!(
        received,
        lost,
        unattributed = aggregator.unattributed,
        "ETW window closed"
    );
    Ok(())
}

fn handle(shared: &Shared, aggregator: &mut AccessAggregator, event: TraceEvent) {
    match event {
        TraceEvent::Access(event) => aggregator.observe(event),
        TraceEvent::ProcessStart { pid, image } => {
            let class = shared
                .with_key(|key| process_class(&image, key))
                .unwrap_or(ProcessClass::Other);
            aggregator.process_seen(pid, class, true);
        }
        TraceEvent::ProcessStop { pid } => aggregator.process_gone(pid),
    }
}

fn flush(shared: &Shared, aggregator: &mut AccessAggregator, elapsed: Duration) {
    let summaries = aggregator.flush(shared.anchor(), elapsed.as_secs().max(1) as u32);
    if summaries.is_empty() {
        return;
    }
    let records: Vec<ObservationRecord> = summaries
        .into_iter()
        .map(ObservationRecord::Access)
        .collect();
    if let Err(error) = shared.spool.lock().unwrap().append_all(&records) {
        tracing::error!(error = %error, "spool access batch failed");
    }
}

/// `(pid, image file name)` for every running process.
pub fn running_processes() -> Vec<(u32, String)> {
    let mut processes = Vec::new();
    // SAFETY: snapshot handle is closed below; entry struct sized as declared.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return processes;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                processes.push((
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..end]),
                ));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    processes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_fit_inside_the_hour() {
        let start = Instant::now();
        for minutes in [1, 10, 30, 60, 90] {
            let plan = plan_hour(start, minutes);
            assert!(plan.window_start >= start);
            assert!(plan.window_start + plan.window_len <= start + Duration::from_secs(3600));
        }
        assert_eq!(plan_hour(start, 60).window_start, start);
        assert!(running_processes()
            .iter()
            .any(|(pid, _)| *pid == std::process::id()));
    }
}
