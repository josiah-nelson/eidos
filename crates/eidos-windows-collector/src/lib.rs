//! Windows observatory collector.
//!
//! One lane set of the `eidos observe` family: a LocalSystem service that
//! reads every local USN journal, samples host and volume shape, optionally
//! traces file access through ETW in bounded windows, and spools only the
//! tokenised, bucketed records defined by `eidos-observe`. Control is a
//! local named pipe; there is no listener, upload, or remote control.
//!
//! Platform-neutral logic (configuration, protocol, change analytics) is
//! compiled and tested everywhere; the native lanes are `cfg(windows)`.

pub mod access;
pub mod analytics;
pub mod cdc;
pub mod client;
pub mod config;
pub mod protocol;

#[cfg(windows)]
pub mod access_lane;
#[cfg(windows)]
pub mod content_probe;
#[cfg(windows)]
pub mod daemon;
#[cfg(windows)]
pub mod enumeration_probe;
#[cfg(windows)]
pub mod etw;
#[cfg(windows)]
pub mod hostfacts;
#[cfg(windows)]
pub mod keystore;
#[cfg(windows)]
pub mod lanes;
#[cfg(windows)]
pub mod log;
#[cfg(windows)]
pub mod object_facts;
pub mod pipe;
#[cfg(windows)]
pub mod resources;
#[cfg(windows)]
pub mod service;
#[cfg(windows)]
pub mod upload;
#[cfg(windows)]
pub mod usn_lane;
#[cfg(windows)]
pub mod volumes;

/// Service name registered with the SCM and the pipe name it listens on.
pub const SERVICE_NAME: &str = "eidos-collector";
pub const PIPE_NAME: &str = r"\\.\pipe\eidos-collector";
pub const DEFAULT_DATA_DIR: &str = r"C:\ProgramData\eidos-collector";
