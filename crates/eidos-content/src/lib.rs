//! Streaming literal-text extraction (SPEC 7.6, ARCHITECTURE 9).
//!
//! `open -> sniff -> decode -> chunk -> hash -> sink`, never holding more
//! than a bounded read buffer plus one chunk in memory. Every chunk carries
//! exact byte and line ranges so snippets and file reads are verifiable.

pub mod chunk;
pub mod extract;
pub mod sniff;

pub use chunk::{Chunk, Chunker, ChunkerConfig};
pub use extract::{extract, Limits, Outcome, SinkFailure, SinkResult};
pub use sniff::{sniff, Encoding, Sniff};

/// Bump when chunking or decoding semantics change so stored chunks are
/// re-extracted.
pub const EXTRACTION_VERSION: u16 = 1;
