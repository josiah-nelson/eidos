//! Archive container recognition shared by the catalog (job priority), the
//! content pipeline (which parser to run), and the UI.

use serde::{Deserialize, Serialize};

/// Container formats whose member inventory eidos can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    /// PKZIP family: `.zip` and the many application formats built on it
    /// (Java/Android packages, NuGet, VSIX, browser extensions, wheels,
    /// EPUB, …). Office Open XML documents are deliberately excluded: their
    /// member names carry no user-facing information and a later release
    /// extracts them as documents.
    Zip,
}

impl ArchiveFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "zip",
        }
    }
}

/// Extensions (lower-case, without the dot) recognised as ZIP containers.
pub const ZIP_EXTENSIONS: &[&str] = &[
    "zip",
    "jar",
    "war",
    "ear",
    "aar",
    "apk",
    "ipa",
    "nupkg",
    "vsix",
    "xpi",
    "crx",
    "whl",
    "egg",
    "epub",
    "oxt",
    "kmz",
    "xap",
    "appx",
    "msix",
    "sublime-package",
    "pk3",
    "pak",
];

/// The archive format a file name announces, if any. Extension-gated on
/// purpose: sniffing every file for a ZIP end record would cost a read of
/// the tail of every object in the corpus.
pub fn archive_format(name: &str) -> Option<ArchiveFormat> {
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    if ext.len() > 16 || ext.contains(['/', '\\']) {
        return None;
    }
    let lower = ext.to_ascii_lowercase();
    ZIP_EXTENSIONS
        .contains(&lower.as_str())
        .then_some(ArchiveFormat::Zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_gate() {
        assert_eq!(archive_format("a.zip"), Some(ArchiveFormat::Zip));
        assert_eq!(archive_format(r"C:\x\tool.JAR"), Some(ArchiveFormat::Zip));
        assert_eq!(archive_format("pkg.nupkg"), Some(ArchiveFormat::Zip));
        assert_eq!(archive_format("doc.docx"), None);
        assert_eq!(archive_format("zip"), None);
        assert_eq!(archive_format("dir.zip/file"), None);
        assert_eq!(archive_format("a.tar.gz"), None);
    }
}
