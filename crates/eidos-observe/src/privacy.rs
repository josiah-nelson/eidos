use crate::schema::{AgeBucket, DepthBucket, ExtensionBucket, SizeBucket};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Secret study key. It intentionally implements neither serialization nor
/// `Debug`; platform code is responsible for storing it in a local keychain.
pub struct StudyKey([u8; 32]);

impl StudyKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn token(&self, domain: &'static str, value: &[u8]) -> ObjectToken {
        let mut input = Vec::with_capacity(domain.len() + value.len() + 1);
        input.extend_from_slice(domain.as_bytes());
        input.push(0);
        input.extend_from_slice(value);
        let digest = blake3::keyed_hash(&self.0, &input);
        ObjectToken::from_digest(digest.as_bytes())
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectToken(String);

impl ObjectToken {
    fn from_digest(digest: &[u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        Self(out)
    }

    pub fn encoded(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ObjectToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObjectToken(REDACTED)")
    }
}

pub fn bucket_size(bytes: u64) -> SizeBucket {
    use SizeBucket::*;
    match bytes {
        0 => Zero,
        1..=1_023 => B1K,
        1_024..=4_095 => B4K,
        4_096..=16_383 => B16K,
        16_384..=65_535 => B64K,
        65_536..=262_143 => B256K,
        262_144..=1_048_575 => B1M,
        1_048_576..=4_194_303 => B4M,
        4_194_304..=16_777_215 => B16M,
        16_777_216..=67_108_863 => B64M,
        67_108_864..=268_435_455 => B256M,
        268_435_456..=1_073_741_823 => B1G,
        _ => Larger,
    }
}

pub fn bucket_age(seconds: u64) -> AgeBucket {
    match seconds {
        0..=1 => AgeBucket::Immediate,
        2..=59 => AgeBucket::Seconds,
        60..=3_599 => AgeBucket::Minutes,
        3_600..=86_399 => AgeBucket::Hours,
        86_400..=604_799 => AgeBucket::Days,
        604_800..=2_419_199 => AgeBucket::Weeks,
        _ => AgeBucket::Older,
    }
}

pub fn bucket_depth(depth: usize) -> DepthBucket {
    match depth {
        0 => DepthBucket::Root,
        1..=2 => DepthBucket::Shallow,
        3..=5 => DepthBucket::Medium,
        6..=10 => DepthBucket::Deep,
        _ => DepthBucket::VeryDeep,
    }
}

pub fn bucket_extension(extension: Option<&str>) -> ExtensionBucket {
    let Some(extension) = extension else {
        return ExtensionBucket::None;
    };
    match extension
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" | "md" | "pdf" | "doc" | "docx" | "rtf" => ExtensionBucket::Document,
        "c" | "h" | "cc" | "cpp" | "go" | "java" | "js" | "py" | "rs" | "ts" => {
            ExtensionBucket::Source
        }
        "zip" | "tar" | "gz" | "7z" | "dmg" => ExtensionBucket::Archive,
        "gif" | "heic" | "jpeg" | "jpg" | "png" | "svg" => ExtensionBucket::Image,
        "aac" | "mov" | "mp3" | "mp4" | "wav" => ExtensionBucket::AudioVideo,
        "app" | "bin" | "dll" | "dylib" | "exe" => ExtensionBucket::Executable,
        "db" | "sqlite" | "sqlite3" => ExtensionBucket::Database,
        "conf" | "ini" | "json" | "plist" | "toml" | "yaml" | "yml" => {
            ExtensionBucket::Configuration
        }
        _ => ExtensionBucket::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_keyed_stable_and_domain_separated() {
        let key = StudyKey::from_bytes([7; 32]);
        let first = key.token("object", b"synthetic/item");
        assert_eq!(first, key.token("object", b"synthetic/item"));
        assert_ne!(first, key.token("subtree", b"synthetic/item"));
        assert_ne!(
            first,
            StudyKey::from_bytes([8; 32]).token("object", b"synthetic/item")
        );
        assert!(!format!("{first:?}").contains(first.encoded()));
    }

    #[test]
    fn buckets_hide_exact_values() {
        assert_eq!(bucket_size(5_000), SizeBucket::B16K);
        assert_eq!(bucket_age(300), AgeBucket::Minutes);
        assert_eq!(bucket_depth(9), DepthBucket::Deep);
        assert_eq!(bucket_extension(Some("RS")), ExtensionBucket::Source);
        assert_eq!(bucket_extension(Some("invented")), ExtensionBucket::Other);
    }
}
