//! Client side of the control pipe, used by `eidos observe`.

use crate::protocol::{encode, read_frame, Request, Response};
use crate::PIPE_NAME;
use std::io::Write;
use std::time::Duration;

/// Send one request and read one response. Opening the pipe requires write
/// access, which the default pipe security grants to administrators and
/// SYSTEM only, so an unelevated caller gets access denied here.
pub fn request(request: &Request) -> anyhow::Result<Response> {
    let mut pipe = open_pipe()?;
    pipe.write_all(&encode(request)?)?;
    pipe.flush()?;
    let body = read_frame(&mut pipe)?;
    Ok(serde_json::from_slice(&body)?)
}

fn open_pipe() -> anyhow::Result<std::fs::File> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE_NAME)
        {
            Ok(file) => return Ok(file),
            // ERROR_PIPE_BUSY: every instance is serving another client.
            Err(error)
                if error.raw_os_error() == Some(231) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "the collector service is not running (no pipe at {PIPE_NAME}); start it with `eidos observe start`"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                anyhow::bail!(
                    "access to the collector pipe was denied; run from an elevated prompt"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn expect_accepted(response: Response) -> anyhow::Result<()> {
    match response {
        Response::Accepted => Ok(()),
        Response::Error { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected collector response: {other:?}"),
    }
}
