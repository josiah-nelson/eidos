//! Platform-neutral contract for bounded, privacy-preserving observations.
//!
//! Native feeds keep their own opaque, versioned cursor. Adapters must emit
//! only the bucketed and tokenised records defined here; raw filesystem and
//! process data has no representation in the durable schema.

pub mod bundle;
pub mod families;
pub mod feed;
pub mod privacy;
pub mod schema;
pub mod spool;

pub use bundle::{inspect_bundle, read_bundle, write_bundle, BundleInspection};
pub use families::*;
pub use feed::{ChangeFeed, FeedBatch, FeedError};
pub use privacy::{
    bucket_age, bucket_depth, bucket_extension, bucket_size, ObjectToken, StudyKey, TokenHasher,
};
pub use schema::*;
pub use spool::{export_bundle, Spool, SpoolLimits, SpoolStats};
