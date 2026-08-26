//! Named-pipe control server. One request and one response per connection;
//! the default pipe security descriptor grants write access (needed to issue
//! a request at all) to administrators and SYSTEM only, and
//! `PIPE_REJECT_REMOTE_CLIENTS` keeps it local.
//!
//! That descriptor still grants *read* access to Everyone, so any local user
//! can occupy a pipe instance without ever sending a frame. Connections are
//! therefore capped, and a connection that has not delivered a complete
//! request within `CLIENT_DEADLINE` has its blocking read cancelled so its
//! slot returns to the pool.

use crate::protocol::{encode, read_frame, Request, Response, MAX_FRAME_BYTES};
use crate::PIPE_NAME;
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, ERROR_PIPE_CONNECTED,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
use windows_sys::Win32::System::IO::CancelSynchronousIo;

/// At most this many connections are served at once. The control plane is an
/// administrative surface, not a throughput path, so a small cap is enough to
/// keep an unprivileged local user from growing threads and pipe instances
/// without bound.
const MAX_CONCURRENT_CLIENTS: usize = 16;

/// A connection that has not delivered a complete request frame within this
/// long has its blocking read cancelled.
#[cfg(not(test))]
const CLIENT_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
const CLIENT_DEADLINE: Duration = Duration::from_millis(750);

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// The live connection counter of the most recent server, so the tests can
/// watch the cap hold and the slots come back.
#[cfg(test)]
static ACTIVE_PROBE: Mutex<Option<Arc<AtomicUsize>>> = Mutex::new(None);

/// A pipe instance handle owned by the server thread.
struct Instance(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: named-pipe handles may be used from any thread; each instance is
// owned by exactly one thread at a time.
unsafe impl Send for Instance {}

/// A client thread that may still be blocked in `read_frame`, together with
/// the duplicated thread handle the reaper needs to cancel that read.
struct LiveClient {
    thread: HANDLE,
    deadline: Instant,
    done: Arc<AtomicBool>,
}

// SAFETY: a thread handle is a kernel object usable from any thread; the
// registry owns exactly one reference to it and closes it once.
unsafe impl Send for LiveClient {}

/// Close and forget the clients that have finished.
fn prune(live: &mut Vec<LiveClient>) {
    live.retain(|client| {
        if client.done.load(Ordering::Acquire) {
            // SAFETY: the duplicated handle is owned here and dropped once.
            unsafe { CloseHandle(client.thread) };
            false
        } else {
            true
        }
    });
}

/// Duplicate the calling thread's pseudo-handle into a real handle the reaper
/// can hold after this thread has moved on.
fn current_thread_handle() -> Option<HANDLE> {
    let mut duplicated: HANDLE = std::ptr::null_mut();
    // SAFETY: both pseudo-handles are valid for the duration of the call and
    // the duplicate becomes owned by the registry.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    (ok != 0).then_some(duplicated)
}

/// Cancel the blocking read of any client past its deadline so its slot is
/// returned, and reap the ones that have finished.
fn reap(clients: Arc<Mutex<Vec<LiveClient>>>, shutdown: Arc<AtomicBool>) {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let stopping = shutdown.load(Ordering::Acquire);
        let mut live = clients.lock().unwrap();
        prune(&mut live);
        let now = Instant::now();
        for client in live.iter() {
            if stopping || now >= client.deadline {
                // SAFETY: the handle is owned by the registry and still open;
                // cancelling a thread with no pending I/O simply fails.
                unsafe { CancelSynchronousIo(client.thread) };
            }
        }
        if stopping && live.is_empty() {
            return;
        }
    }
}

/// Start accepting connections. Returns once the first pipe instance exists,
/// so a client that races the service start finds the pipe.
pub fn serve(shutdown: Arc<AtomicBool>, handler: Handler) -> std::io::Result<JoinHandle<()>> {
    let first = create_instance()?;
    let clients: Arc<Mutex<Vec<LiveClient>>> = Arc::new(Mutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    #[cfg(test)]
    {
        *ACTIVE_PROBE.lock().unwrap() = Some(active.clone());
    }
    {
        let clients = clients.clone();
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("collector-pipe-reaper".into())
            .spawn(move || reap(clients, shutdown))?;
    }
    std::thread::Builder::new()
        .name("collector-pipe".into())
        .spawn(move || {
            let mut instance = Some(first);
            while !shutdown.load(Ordering::Acquire) {
                let Instance(handle) = match instance.take() {
                    Some(handle) => handle,
                    None => match create_instance() {
                        Ok(handle) => handle,
                        Err(error) => {
                            tracing::warn!(error = %error, "cannot create a pipe instance");
                            std::thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    },
                };
                // SAFETY: handle from CreateNamedPipeW; blocking connect.
                let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
                    || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                if !connected || shutdown.load(Ordering::Acquire) {
                    // SAFETY: handle owned here.
                    unsafe { CloseHandle(handle) };
                    continue;
                }
                prune(&mut clients.lock().unwrap());
                if active.load(Ordering::Acquire) >= MAX_CONCURRENT_CLIENTS {
                    tracing::warn!(
                        cap = MAX_CONCURRENT_CLIENTS,
                        "pipe connection refused: too many in flight"
                    );
                    // SAFETY: handle owned here and not yet handed to a client.
                    unsafe {
                        DisconnectNamedPipe(handle);
                        CloseHandle(handle)
                    };
                    continue;
                }
                active.fetch_add(1, Ordering::AcqRel);
                // SAFETY: the pipe handle is a valid file handle owned by this thread.
                let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
                let handler = handler.clone();
                let done = Arc::new(AtomicBool::new(false));
                let registry = clients.clone();
                let active_slot = active.clone();
                let entry = done.clone();
                let spawned = std::thread::Builder::new()
                    .name("collector-pipe-client".into())
                    .spawn(move || {
                        if let Some(thread) = current_thread_handle() {
                            registry.lock().unwrap().push(LiveClient {
                                thread,
                                deadline: Instant::now() + CLIENT_DEADLINE,
                                done: entry.clone(),
                            });
                        }
                        serve_client(file, handler);
                        entry.store(true, Ordering::Release);
                        active_slot.fetch_sub(1, Ordering::AcqRel);
                    });
                if spawned.is_err() {
                    done.store(true, Ordering::Release);
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })
}

/// Unblock a server thread waiting in `ConnectNamedPipe` after the shutdown
/// flag is set.
pub fn poke() {
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME);
}

fn create_instance() -> std::io::Result<Instance> {
    let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated name; default security attributes.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            MAX_FRAME_BYTES as u32,
            MAX_FRAME_BYTES as u32,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Instance(handle))
}

fn serve_client(mut file: std::fs::File, handler: Handler) {
    let response = match read_frame(&mut file) {
        Ok(body) => match serde_json::from_slice::<Request>(&body) {
            Ok(request) => handler(request),
            Err(error) => Response::Error {
                message: format!("malformed request: {error}"),
            },
        },
        Err(error) => Response::Error {
            message: format!("unreadable request: {error}"),
        },
    };
    match encode(&response) {
        Ok(frame) => {
            let _ = file.write_all(&frame);
            let _ = file.flush();
        }
        Err(error) => tracing::warn!(error = %error, "response exceeds the frame bound"),
    }
    // SAFETY: the file wraps a connected pipe instance.
    unsafe {
        FlushFileBuffers(file.as_raw_handle() as _);
        DisconnectNamedPipe(file.as_raw_handle() as _);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client;

    /// Both tests bind the one machine-wide pipe name, so they must not run
    /// at the same time: concurrent servers would answer each other's clients.
    static PIPE: Mutex<()> = Mutex::new(());

    /// Take the pipe lock, ignoring poisoning from an unrelated failure.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        PIPE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A local user with only read access can open the pipe and never send a
    /// frame. Those connections must stay capped, must be reclaimed once their
    /// deadline passes, and must not keep an administrator out of the control
    /// plane.
    #[test]
    fn stalled_readers_are_capped_and_cancelled() {
        let _guard = exclusive();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler: Handler = Arc::new(|_| Response::Accepted);
        let server = match serve(shutdown.clone(), handler) {
            Ok(server) => server,
            // Another collector owns the pipe on this host.
            Err(error) if error.raw_os_error() == Some(5) => return,
            Err(error) => panic!("{error}"),
        };
        let active = ACTIVE_PROBE.lock().unwrap().clone().unwrap();

        // Hold connections open without ever writing a request. Each open
        // races the accept loop creating the next instance, so retry on
        // ERROR_PIPE_BUSY to actually reach the cap.
        let mut stalled: Vec<std::fs::File> = Vec::new();
        let fill = Instant::now() + Duration::from_secs(5);
        let mut peak = 0usize;
        while stalled.len() < MAX_CONCURRENT_CLIENTS * 2 && Instant::now() < fill {
            match std::fs::OpenOptions::new().read(true).open(PIPE_NAME) {
                Ok(file) => stalled.push(file),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
            peak = peak.max(active.load(Ordering::Acquire));
            if peak >= MAX_CONCURRENT_CLIENTS {
                break;
            }
        }
        assert!(
            peak <= MAX_CONCURRENT_CLIENTS,
            "served {peak} at once, above the {MAX_CONCURRENT_CLIENTS} cap"
        );

        // The deadline returns the slots even though no client sent anything.
        let drained = Instant::now() + Duration::from_secs(10);
        while active.load(Ordering::Acquire) > 0 && Instant::now() < drained {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            active.load(Ordering::Acquire),
            0,
            "stalled connections were never reclaimed"
        );

        // And the control plane still answers.
        assert_eq!(
            client::request(&Request::Status).unwrap(),
            Response::Accepted
        );

        drop(stalled);
        shutdown.store(true, Ordering::Release);
        poke();
        let _ = server.join();
    }

    #[test]
    fn serves_a_status_request_and_rejects_malformed_frames() {
        let _guard = exclusive();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler: Handler = Arc::new(|request| match request {
            Request::Mark { label } => Response::Error { message: label },
            _ => Response::Accepted,
        });
        let server = match serve(shutdown.clone(), handler) {
            Ok(server) => server,
            // Another collector owns the pipe on this host; the protocol
            // itself is covered by its own tests.
            Err(error) if error.raw_os_error() == Some(5) => return,
            Err(error) => panic!("{error}"),
        };
        assert_eq!(
            client::request(&Request::Status).unwrap(),
            Response::Accepted
        );
        assert_eq!(
            client::request(&Request::Mark {
                label: "echo".into()
            })
            .unwrap(),
            Response::Error {
                message: "echo".into()
            }
        );
        let mut raw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE_NAME)
            .unwrap();
        raw.write_all(&encode(&serde_json::json!({"request": "bogus"})).unwrap())
            .unwrap();
        let body = read_frame(&mut raw).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Response>(&body).unwrap(),
            Response::Error { .. }
        ));
        shutdown.store(true, Ordering::Release);
        poke();
        server.join().unwrap();
    }
}
