use crate::protocol::{Request, Response};
use security_framework::passwords::{get_generic_password, set_generic_password};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const KEYCHAIN_SERVICE: &str = "com.jnel.eidos.collector.study-key";
const KEYCHAIN_ACCOUNT: &str = "eidos-observe";

pub fn init_key(force: bool) -> anyhow::Result<bool> {
    if !force && get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).is_ok() {
        return Ok(false);
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key)?;
    set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &key)?;
    Ok(true)
}

pub fn study_key() -> anyhow::Result<[u8; 32]> {
    let value = get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
    value
        .try_into()
        .map_err(|_| anyhow::anyhow!("study key has an invalid length; run observe init --force"))
}

pub fn request(socket: &Path, request: &Request) -> anyhow::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

pub fn load_session_key(socket: &Path) -> anyhow::Result<()> {
    let key = study_key()?;
    match request(
        socket,
        &Request::SessionKey {
            bytes: key.to_vec(),
        },
    )? {
        Response::Accepted => Ok(()),
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected collector response"),
    }
}
