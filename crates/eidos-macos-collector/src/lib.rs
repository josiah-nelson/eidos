#[cfg(target_os = "macos")]
pub mod client;
#[cfg(target_os = "macos")]
pub mod daemon;
#[cfg(target_os = "macos")]
mod endpoint_security;
pub mod protocol;

pub const DEFAULT_DATA_DIR: &str = "/var/db/eidos-collector";
pub const DEFAULT_SOCKET: &str = "/var/run/eidos-collector.sock";
pub const DEFAULT_EXPORT_DIR: &str = "/var/db/eidos-collector/exports";
