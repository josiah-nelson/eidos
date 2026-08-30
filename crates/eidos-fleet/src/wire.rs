//! Versioned framing for the sync session.
//!
//! A frame is a 4-byte big-endian length followed by that many bytes of
//! JSON encoding one [`Message`]. The length is checked against the
//! receiver's own limit before a single byte of payload is allocated: a
//! peer-declared size is never a reason to allocate. Protocol version and
//! features are negotiated in the first message of each direction
//! ([`Hello`]); everything after it belongs to one of the message families
//! the metrics count.

use eidos_catalog::fleet::NodeId;
use eidos_catalog::replica::RemoteSourceDescriptor;
use eidos_catalog::sync::{SyncBatch, SyncRow};
use eidos_domain::SourceId;
use eidos_sync::identity::{ChainHash, SourceEpoch};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

/// The one protocol version this build speaks. A peer that shares no
/// version with us is refused before any payload is processed.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame this side will read or write.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Rows per materialized batch.
pub const DEFAULT_BATCH_ROWS: u32 = 2_000;
/// Encoded bytes per materialized batch; a batch over it is halved and
/// re-materialized.
pub const DEFAULT_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// Bytes a consumer lets a shipper have in flight before it must wait for
/// acknowledgements.
pub const DEFAULT_CREDIT_BYTES: u64 = 16 * 1024 * 1024;

/// Which side of the fleet a peer claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Node,
    Central,
}

/// First message in each direction after the TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub node_id: NodeId,
    pub name: String,
    pub platform: String,
    pub role: Role,
    /// Random per connection; with the initiator's node id it identifies the
    /// connection for the duplicate-session tie-break.
    pub nonce: u64,
    pub versions: Vec<u32>,
    #[serde(default)]
    pub features: Vec<String>,
    /// Largest frame the sender will accept.
    pub max_frame_bytes: u64,
    /// Bytes the sender allows the peer to have in flight towards it.
    pub credit_bytes: u64,
}

/// An enrollment credential that clears its backing allocation on drop and
/// never reveals its contents through `Debug` formatting.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnrollmentSecret(String);

impl EnrollmentSecret {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for EnrollmentSecret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for EnrollmentSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Drop for EnrollmentSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Everything that crosses the wire after the TLS handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Goodbye {
        reason: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Enrollment: sent by a node whose certificate is not yet in the
    /// central's roster, as the only message it may send.
    Enroll {
        secret: EnrollmentSecret,
        name: String,
        platform: String,
    },
    Enrolled {
        node_id: NodeId,
        name: String,
    },
    EnrollRejected {
        reason: String,
    },
    /// A shipper offers one of its sources (the protocol's per-source
    /// `Hello`).
    Offer {
        descriptor: RemoteSourceDescriptor,
        epoch: SourceEpoch,
        head_seq: u64,
        head_chain: ChainHash,
        compacted_through: u64,
        image_version: u32,
    },
    Resume {
        source: SourceId,
        epoch: SourceEpoch,
        after_seq: u64,
        requires_repair: bool,
    },
    FullResync {
        source: SourceId,
        epoch: SourceEpoch,
    },
    Batch(SyncBatch),
    Ack {
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
    },
    Rejected {
        source: SourceId,
        reason: String,
    },
    RepairOffer {
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaf_hashes: Vec<[u8; 32]>,
    },
    RepairRequest {
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaves: Vec<u32>,
    },
    /// Rows of a subset of the requested leaves; `final_part` closes the
    /// repair. Every part moves the consumer's cursor to `through_seq`.
    RepairRows {
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaves: Vec<u32>,
        rows: Vec<SyncRow>,
        final_part: bool,
    },
}

/// Message families, for byte accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Control,
    Catalog,
    Repair,
}

impl Message {
    pub fn family(&self) -> Family {
        match self {
            Message::Batch(_) | Message::Ack { .. } | Message::Offer { .. } => Family::Catalog,
            Message::RepairOffer { .. }
            | Message::RepairRequest { .. }
            | Message::RepairRows { .. } => Family::Repair,
            _ => Family::Control,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Message::Hello(_) => "hello",
            Message::Goodbye { .. } => "goodbye",
            Message::Ping { .. } => "ping",
            Message::Pong { .. } => "pong",
            Message::Enroll { .. } => "enroll",
            Message::Enrolled { .. } => "enrolled",
            Message::EnrollRejected { .. } => "enroll_rejected",
            Message::Offer { .. } => "offer",
            Message::Resume { .. } => "resume",
            Message::FullResync { .. } => "full_resync",
            Message::Batch(_) => "batch",
            Message::Ack { .. } => "ack",
            Message::Rejected { .. } => "rejected",
            Message::RepairOffer { .. } => "repair_offer",
            Message::RepairRequest { .. } => "repair_request",
            Message::RepairRows { .. } => "repair_rows",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("malformed frame: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("connection closed")]
    Closed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn encode(message: &Message) -> Result<Vec<u8>, FrameError> {
    Ok(serde_json::to_vec(message)?)
}

pub fn decode(bytes: &[u8]) -> Result<Message, FrameError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Write one frame. Returns the bytes put on the wire, header included.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    payload: &[u8],
    max: usize,
) -> Result<usize, FrameError> {
    if payload.len() > max {
        return Err(FrameError::TooLarge {
            len: payload.len(),
            max,
        });
    }
    let len = (payload.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(payload.len() + 4)
}

/// Read one frame, refusing anything over `max` before allocating for it.
pub async fn read_frame<R: AsyncReadExt + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<(Message, usize), FrameError> {
    let mut header = [0u8; 4];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut payload = Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Closed
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok((decode(&payload)?, len + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_oversized_frames_are_refused_before_allocation() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let msg = Message::Ping { nonce: 42 };
        let bytes = encode(&msg).unwrap();
        let n = write_frame(&mut a, &bytes, 1024).await.unwrap();
        assert_eq!(n, bytes.len() + 4);
        let (got, read) = read_frame(&mut b, 1024).await.unwrap();
        assert_eq!(got, msg);
        assert_eq!(read, n);

        // A header claiming a huge frame is rejected without reading it.
        a.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        match read_frame(&mut b, 1024).await {
            Err(FrameError::TooLarge { len, max }) => {
                assert_eq!(len, u32::MAX as usize);
                assert_eq!(max, 1024);
            }
            other => panic!("{other:?}"),
        }
        // Writing over the limit is refused too.
        assert!(matches!(
            write_frame(&mut a, &vec![0u8; 2048], 1024).await,
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn unknown_messages_are_malformed_not_panics() {
        assert!(matches!(
            decode(br#"{"t":"teleport","x":1}"#),
            Err(FrameError::Malformed(_))
        ));
        assert!(matches!(decode(b"\xff\xfe"), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn enrollment_secrets_are_redacted_from_debug_output() {
        let secret = "sensitive-enrollment-secret";
        let message = Message::Enroll {
            secret: secret.to_string().into(),
            name: "node".into(),
            platform: "windows".into(),
        };
        assert!(!format!("{message:?}").contains(secret));
        assert!(format!("{message:?}").contains("REDACTED"));
    }
}
