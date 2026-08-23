//! Identity model.
//!
//! External identifiers are opaque. Internal native identity (volume serial +
//! file ID) is preserved so that renames and hard links are recognised.
//!
//! See ARCHITECTURE.md section 5.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! opaque_i64_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub i64);

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                crate::json::i64_string::serialize(&self.0, serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                crate::json::i64_string::deserialize(deserializer).map(Self)
            }
        }

        impl $name {
            pub const fn new(v: i64) -> Self {
                Self(v)
            }
            pub const fn raw(self) -> i64 {
                self.0
            }
            /// Opaque external form, e.g. `o:42`.
            pub fn external(self) -> String {
                format!("{}:{}", $prefix, self.0)
            }
            pub fn parse_external(s: &str) -> Option<Self> {
                let rest = s.strip_prefix(concat!($prefix, ":"))?;
                rest.parse().ok().map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", $prefix, self.0)
            }
        }
    };
}

opaque_i64_id!(
    /// A machine that owns sources. v0.5 has exactly one local host.
    HostId,
    "h"
);
opaque_i64_id!(
    /// A configured filesystem scope. Stable across drive letters and agents.
    SourceId,
    "s"
);
opaque_i64_id!(
    /// A physical or logical volume observed beneath a source.
    VolumeId,
    "v"
);
opaque_i64_id!(
    /// A filesystem object (file, directory, reparse point, virtual member).
    ObjectId,
    "o"
);
opaque_i64_id!(
    /// A directory entry: (parent object, exact name) at an observed generation.
    EntryId,
    "e"
);
opaque_i64_id!(
    /// A scan generation within a source.
    ScanGeneration,
    "g"
);
opaque_i64_id!(
    /// A durable job.
    JobId,
    "j"
);

/// Native identity of an object on the source filesystem.
///
/// On NTFS/ReFS this is the volume serial plus the 128-bit file ID. Other
/// filesystems may only provide a 64-bit ID or nothing at all, in which case
/// the scanner synthesises a fallback identity with lower confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NativeIdentity {
    pub volume_serial: u64,
    /// 128-bit file reference. For 64-bit IDs the high half is zero.
    pub file_id_high: u64,
    pub file_id_low: u64,
    pub confidence: IdentityConfidence,
}

impl NativeIdentity {
    pub fn file_id_u128(&self) -> u128 {
        ((self.file_id_high as u128) << 64) | self.file_id_low as u128
    }

    pub fn from_u128(volume_serial: u64, id: u128, confidence: IdentityConfidence) -> Self {
        Self {
            volume_serial,
            file_id_high: (id >> 64) as u64,
            file_id_low: id as u64,
            confidence,
        }
    }
}

/// How much the scanner trusts the native identity to survive renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityConfidence {
    /// Stable native file ID (NTFS/ReFS 128-bit or 64-bit IDs).
    Native,
    /// Some native ID but known to be reusable or unstable (many SMB servers).
    Weak,
    /// Derived from path + size + timestamps; replaced on any path change.
    PathDerived,
}

impl IdentityConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Weak => "weak",
            Self::PathDerived => "path_derived",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "native" => Self::Native,
            "weak" => Self::Weak,
            "path_derived" => Self::PathDerived,
            _ => return None,
        })
    }
}

/// BLAKE3 content hash of the full object bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentId(pub [u8; 32]);

impl ContentId {
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentId({})", self.to_hex())
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for ContentId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContentId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ContentId::from_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid content id hex"))
    }
}

/// Identity of an extracted chunk: object generation, extraction version,
/// and ordinal within the extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkId {
    pub object_id: ObjectId,
    pub object_generation: u32,
    pub extraction_version: u16,
    pub ordinal: u32,
}

impl ChunkId {
    /// Stable string key suitable for search index document identity.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.object_id.0, self.object_generation, self.extraction_version, self.ordinal
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_ids_roundtrip() {
        let o = ObjectId(42);
        assert_eq!(o.external(), "o:42");
        assert_eq!(ObjectId::parse_external("o:42"), Some(o));
        assert_eq!(ObjectId::parse_external("s:42"), None);
        assert_eq!(serde_json::to_string(&o).unwrap(), "\"42\"");
        assert_eq!(serde_json::from_str::<ObjectId>("42").unwrap(), o);
    }

    #[test]
    fn content_id_hex() {
        let c = ContentId([0xab; 32]);
        let hex = c.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(ContentId::from_hex(&hex), Some(c));
        let json = serde_json::to_string(&c).unwrap();
        let back: ContentId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn native_identity_u128() {
        let n = NativeIdentity::from_u128(7, (5u128 << 64) | 9, IdentityConfidence::Native);
        assert_eq!(n.file_id_high, 5);
        assert_eq!(n.file_id_low, 9);
        assert_eq!(n.file_id_u128(), (5u128 << 64) | 9);
    }
}
