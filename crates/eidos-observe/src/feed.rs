use crate::schema::{FeedCursor, ObservationRecord};

#[derive(Debug)]
pub struct FeedBatch {
    pub cursor: FeedCursor,
    pub records: Vec<ObservationRecord>,
    pub coalesced: u64,
    pub dropped: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("native feed overflowed")]
    Overflow,
    #[error("native feed root changed")]
    RootChanged,
    #[error("native feed is unavailable: {0}")]
    Unavailable(String),
    #[error("native feed failed: {0}")]
    Other(String),
}

/// Native feeds deliberately do not share cursor semantics.
pub trait ChangeFeed: Send {
    fn kind(&self) -> crate::schema::FeedKind;
    fn resume(&mut self, cursor: Option<&FeedCursor>) -> Result<(), FeedError>;
    fn next_batch(&mut self) -> Result<FeedBatch, FeedError>;
}
