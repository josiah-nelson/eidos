//! Shared domain contracts for the filesystem indexer.
//!
//! Everything in this crate is part of the public semantic contract shared by
//! the service, CLI, web UI, saved queries, and (later) the NLQ adapter and MCP
//! server. Changes here are schema changes and must be versioned.

pub mod bench;
pub mod classify;
pub mod ids;
pub mod query;
pub mod result;
pub mod state;
pub mod time;

pub use classify::*;
pub use ids::*;
pub use query::*;
pub use result::*;
pub use state::*;
pub use time::*;

/// Version of the public query/result schema. Bump on incompatible changes.
pub const SCHEMA_VERSION: u32 = 1;
