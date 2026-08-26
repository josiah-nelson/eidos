//! Named-pipe control server. One request and one response per connection;
//! the default pipe security descriptor grants write access (needed to open
//! the pipe at all) to administrators and SYSTEM only, and
//! `PIPE_REJECT_REMOTE_CLIENTS` keeps it local.

use crate::protocol::{encode, read_frame, Request, Response, MAX_FRAME_BYTES};
use crate::PIPE_NAME;
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// A pipe instance handle owned by the server thread.
struct Instance(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: named-pipe handles may be used from any thread; each instance is
// owned by exactly one thread at a time.
unsafe impl Send for Instance {}

/// Start accepting connections. Returns once the first pipe instance exists,
/// so a client that races the service start finds the pipe.
pub fn serve(shutdown: Arc<AtomicBool>, handler: Handler) -> std::io::Result<JoinHandle<()>> {
    let first = create_instance()?;
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
                // SAFETY: the pipe handle is a valid file handle owned by this thread.
                let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
                let handler = handler.clone();
                let _ = std::thread::Builder::new()
                    .name("collector-pipe-client".into())
                    .spawn(move || serve_client(file, handler));
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

    #[test]
    fn serves_a_status_request_and_rejects_malformed_frames() {
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
