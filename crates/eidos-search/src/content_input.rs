//! Bound text waiting for Tantivy's indexing threads. Its writer budget covers
//! segment construction, while the upstream channel is bounded by document
//! count; thousands of three-field text documents can exceed that budget.

use crate::{Result, SearchError};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tantivy::schema::Document;
use tantivy::TantivyDocument;

pub const CONTENT_INPUT_MEMORY_BYTES: usize = 16 * 1024 * 1024;
const INPUT_WAIT: Duration = Duration::from_secs(5);
/// Covers the fixed field metadata, document, reservation and channel entry.
const DOCUMENT_OVERHEAD: usize = 1024;

#[derive(Default)]
struct Usage {
    bytes: usize,
    peak: usize,
}

pub(crate) struct InputBudget {
    limit: usize,
    usage: Mutex<Usage>,
    ready: Condvar,
}

impl InputBudget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            usage: Mutex::new(Usage::default()),
            ready: Condvar::new(),
        })
    }

    /// Reserve before allocating the three text fields. The fixed extra space
    /// avoids geometric Vec growth and accounts conservatively for metadata.
    ///
    /// `TantivyDocument` is `CompactDoc`, whose `with_capacity` reserves that
    /// many *bytes* of serialized value data (its field-value list is sized
    /// separately), so the preallocation and the reservation describe the same
    /// quantity. `document_capacity_is_a_byte_reservation` pins that meaning.
    pub(crate) fn document(self: &Arc<Self>, text_bytes: usize) -> Result<InputDocument> {
        let capacity = text_bytes
            .checked_mul(3)
            .and_then(|n| n.checked_add(256))
            .ok_or_else(|| SearchError::Other("content input size overflow".into()))?;
        let bytes = capacity
            .checked_add(DOCUMENT_OVERHEAD)
            .ok_or_else(|| SearchError::Other("content input size overflow".into()))?;
        let reservation = self.reserve(bytes, INPUT_WAIT)?;
        Ok(InputDocument {
            document: TantivyDocument::with_capacity(capacity),
            _reservation: reservation,
        })
    }

    fn reserve(self: &Arc<Self>, bytes: usize, timeout: Duration) -> Result<Reservation> {
        if bytes > self.limit {
            return Err(SearchError::Other(
                "content chunk exceeds the indexing input memory budget".into(),
            ));
        }
        let started = Instant::now();
        let mut usage = self.usage.lock();
        while bytes > self.limit - usage.bytes {
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Err(SearchError::Other(
                    "content indexing input did not drain within its deadline".into(),
                ));
            };
            self.ready.wait_for(&mut usage, remaining);
        }
        usage.bytes += bytes;
        usage.peak = usage.peak.max(usage.bytes);
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> (usize, usize) {
        let usage = self.usage.lock();
        (usage.bytes, usage.peak)
    }
}

struct Reservation {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut usage = self.budget.usage.lock();
        usage.bytes -= self.bytes;
        self.budget.ready.notify_all();
    }
}

/// Tantivy owns this reservation until it consumes or discards the document,
/// including error/shutdown paths. No separate acknowledgement can be lost.
pub(crate) struct InputDocument {
    pub(crate) document: TantivyDocument,
    _reservation: Reservation,
}

impl Document for InputDocument {
    type Value<'a> = <TantivyDocument as Document>::Value<'a>;
    type FieldsValuesIter<'a> = <TantivyDocument as Document>::FieldsValuesIter<'a>;

    fn iter_fields_and_values(&self) -> Self::FieldsValuesIter<'_> {
        self.document.iter_fields_and_values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field count would preallocate one entry per byte of text here; a byte
    /// capacity holds the three copies the reservation paid for. Pin the
    /// meaning so a Tantivy upgrade cannot silently change what is reserved.
    #[test]
    fn document_capacity_is_a_byte_reservation() {
        let mut schema = tantivy::schema::Schema::builder();
        let number = schema.add_u64_field("n", tantivy::schema::FAST);
        let text_field = schema.add_text_field("t", tantivy::schema::TEXT);
        let budget = InputBudget::new(CONTENT_INPUT_MEMORY_BYTES);
        let text = "queued content input needle ".repeat(600);
        let mut input = budget.document(text.len()).unwrap();
        let reserved = input.document.node_data.capacity();
        assert!(
            reserved >= 3 * text.len(),
            "{reserved} bytes reserved for {} bytes of text",
            3 * text.len()
        );
        // The same four counters and three text copies `add_chunks` writes.
        for _ in 0..4 {
            input.document.add_u64(number, u64::MAX);
        }
        for _ in 0..3 {
            input.document.add_text(text_field, &text);
        }
        assert_eq!(
            input.document.node_data.capacity(),
            reserved,
            "a filled document must not outgrow the bytes reserved for it"
        );
        assert!(input.document.node_data.len() <= reserved);
        drop(input);
        assert_eq!(budget.usage().0, 0);
    }
    #[test]
    fn capacity_waits_for_owned_document_release_and_rejects_oversize() {
        let budget = InputBudget::new(4096);
        let first = budget.document(700).unwrap();
        assert!(budget.document(usize::MAX).is_err());
        assert!(budget.document(4096).is_err());
        let (sent, received) = std::sync::mpsc::channel();
        let worker_budget = budget.clone();
        let worker = std::thread::spawn(move || {
            let next = worker_budget.document(700).unwrap();
            sent.send(()).unwrap();
            next
        });
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(worker.join().unwrap());
        let (used, peak) = budget.usage();
        assert_eq!(used, 0);
        assert!(peak <= 4096);
    }

    #[test]
    fn stalled_consumer_times_out_without_losing_capacity() {
        let budget = InputBudget::new(4096);
        let held = budget.reserve(4096, INPUT_WAIT).unwrap();
        let error = budget.reserve(1, Duration::from_millis(20)).err().unwrap();
        assert!(error.to_string().contains("deadline"));
        assert_eq!(budget.usage().0, 4096);
        drop(held);
        assert_eq!(budget.usage().0, 0);
        assert!(budget.reserve(4096, INPUT_WAIT).is_ok());
    }
}
