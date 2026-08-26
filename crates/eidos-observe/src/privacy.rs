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
        let mut hasher = self.hasher(domain);
        hasher.update(value);
        hasher.finish()
    }

    /// Streaming form of [`StudyKey::token`] for large inputs such as file
    /// content; the result equals `token(domain, all_bytes)`.
    pub fn hasher(&self, domain: &'static str) -> TokenHasher {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        hasher.update(domain.as_bytes());
        hasher.update(&[0]);
        TokenHasher(hasher)
    }
}

pub struct TokenHasher(blake3::Hasher);

impl TokenHasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(&self) -> ObjectToken {
        ObjectToken::from_digest(self.0.finalize().as_bytes())
    }

    /// Raw keyed digest for in-memory comparison (never persisted directly).
    pub fn finish_digest(&self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
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
        "txt" | "md" | "pdf" | "doc" | "docx" | "rtf" | "odt" | "xls" | "xlsx" | "ppt" | "pptx"
        | "csv" | "epub" | "one" | "pages" | "numbers" | "key" => ExtensionBucket::Document,
        "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "go" | "java" | "kt" | "js" | "jsx" | "ts"
        | "tsx" | "py" | "rs" | "rb" | "php" | "swift" | "m" | "sh" | "ps1" | "psm1" | "bat"
        | "cmd" | "sql" | "vue" | "html" | "css" | "scss" | "xml" | "proto" => {
            ExtensionBucket::Source
        }
        "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" | "xz" | "bz2" | "zst" | "cab" | "dmg"
        | "jar" | "nupkg" | "whl" => ExtensionBucket::Archive,
        "gif" | "heic" | "jpeg" | "jpg" | "png" | "svg" | "webp" | "bmp" | "tif" | "tiff"
        | "psd" | "ico" | "raw" | "cr2" | "nef" => ExtensionBucket::Image,
        "aac" | "mov" | "mp3" | "mp4" | "wav" | "mkv" | "avi" | "flac" | "m4a" | "webm" | "ogg"
        | "wmv" | "wma" => ExtensionBucket::AudioVideo,
        "app" | "bin" | "dll" | "dylib" | "so" | "exe" | "sys" | "msi" | "msix" | "appx"
        | "com" | "scr" => ExtensionBucket::Executable,
        "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" | "ldb" | "edb" | "sdf" | "wal" => {
            ExtensionBucket::Database
        }
        "conf" | "ini" | "json" | "plist" | "toml" | "yaml" | "yml" | "cfg" | "reg"
        | "properties" | "env" => ExtensionBucket::Configuration,
        "o" | "obj" | "rlib" | "rmeta" | "pdb" | "ilk" | "lib" | "a" | "class" | "pyc" | "d"
        | "exp" | "tlog" | "idb" | "pch" | "ipch" | "cache" | "node" => ExtensionBucket::Build,
        "vhd" | "vhdx" | "avhdx" | "vmdk" | "vdi" | "qcow2" | "iso" | "img" | "wim" | "esd"
        | "vmem" | "vmsn" | "vsv" => ExtensionBucket::DiskImage,
        "log" | "etl" | "evtx" | "trace" | "dmp" => ExtensionBucket::Log,
        "tmp" | "temp" | "bak" | "swp" | "part" | "crdownload" | "partial" | "lock" | "lck"
        | "download" | "old" | "orig" => ExtensionBucket::Temporary,
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
