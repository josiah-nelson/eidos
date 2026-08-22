//! Coarse file-kind classification by extension.
//!
//! This is *not* content sniffing; it is the cheap, metadata-only hint used
//! for facets, scheduling estimates, and the corpus profiler. Content policy
//! and binary sniffing (eidos-content) make the authoritative decision.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Plain text, Markdown, config, logs.
    Text,
    /// Source code.
    Code,
    /// Structured data (JSON, XML, CSV, YAML...).
    Data,
    /// Log-like files that tend to be large.
    Log,
    /// HTML and similar markup.
    Markup,
    /// Office documents, PDF (content extraction is v1).
    Document,
    /// Archives (ZIP manifest in v0.5).
    Archive,
    /// VM disks, ISOs, other disk images.
    DiskImage,
    /// Images, audio, video.
    Media,
    /// Executables, libraries, debug symbols.
    Binary,
    /// Database and other opaque data stores.
    Database,
    /// Unknown; extensionless files are sniffed.
    Unknown,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Code => "code",
            Self::Data => "data",
            Self::Log => "log",
            Self::Markup => "markup",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::DiskImage => "disk_image",
            Self::Media => "media",
            Self::Binary => "binary",
            Self::Database => "database",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this kind is a literal-text candidate for v0.5 content indexing
    /// (subject to sniffing).
    pub fn is_text_candidate(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Code | Self::Data | Self::Log | Self::Markup | Self::Unknown
        )
    }

    /// Classify by lowercase extension (without dot). Empty = extensionless.
    pub fn from_extension(ext: &str) -> FileKind {
        use FileKind::*;
        match ext {
            "" => Unknown,
            "txt" | "md" | "markdown" | "rst" | "adoc" | "text" | "ini" | "cfg" | "conf"
            | "config" | "properties" | "env" | "toml" | "nfo" | "readme" | "lst" | "asc"
            | "srt" | "vtt" | "tsv" | "diff" | "patch" | "license" | "1" | "man"
            | "editorconfig" | "gitignore" | "gitattributes" | "dockerignore" | "npmignore" => Text,
            "log" | "clef" | "trace" | "tlog" | "err" | "out" | "etl" => Log,
            "cs" | "csx" | "vb" | "fs" | "fsx" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp"
            | "hh" | "hxx" | "rs" | "go" | "java" | "kt" | "kts" | "scala" | "py" | "pyi"
            | "rb" | "php" | "pl" | "pm" | "lua" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx"
            | "swift" | "m" | "mm" | "dart" | "sql" | "ps1" | "psm1" | "psd1" | "bat" | "cmd"
            | "sh" | "bash" | "zsh" | "fish" | "r" | "jl" | "groovy" | "gradle" | "cmake"
            | "mak" | "mk" | "make" | "asm" | "s" | "pas" | "vbs" | "wsf" | "ahk" | "au3"
            | "tcl" | "ex" | "exs" | "erl" | "hrl" | "clj" | "cljs" | "elm" | "hs" | "ml"
            | "mli" | "nim" | "zig" | "v" | "sv" | "vhdl" | "proto" | "thrift" | "graphql"
            | "gql" | "sln" | "csproj" | "vbproj" | "fsproj" | "vcxproj" | "props" | "targets"
            | "nuspec" | "razor" | "cshtml" | "vue" | "svelte" | "css" | "scss" | "sass"
            | "less" | "styl" => Code,
            "json" | "jsonl" | "ndjson" | "xml" | "xsd" | "xsl" | "xslt" | "yaml" | "yml"
            | "csv" | "plist" | "manifest" | "resx" | "settings" | "reg" | "har" | "geojson"
            | "wsdl" | "svg" | "rdf" | "owl" | "ttl" | "nt" | "jsonc" | "json5" | "avsc"
            | "edmx" | "dtd" | "xaml" | "config.xml" | "inf" | "ics" | "vcf" | "pem" | "crt"
            | "cer" | "key" | "pub" => Data,
            "html" | "htm" | "xhtml" | "mht" | "mhtml" | "shtml" => Markup,
            "pdf" | "doc" | "docx" | "docm" | "dot" | "dotx" | "xls" | "xlsx" | "xlsm" | "xlsb"
            | "ppt" | "pptx" | "pptm" | "odt" | "ods" | "odp" | "rtf" | "epub" | "msg" | "eml"
            | "one" | "vsd" | "vsdx" | "pub_doc" | "xps" | "oxps" => Document,
            "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "tbz" | "xz" | "txz" | "zst"
            | "lz" | "lz4" | "cab" | "jar" | "war" | "ear" | "nupkg" | "vsix" | "apk" | "ipa"
            | "whl" | "egg" | "crx" | "xpi" | "msix" | "appx" | "wim" | "esd" => Archive,
            "vhd" | "vhdx" | "vmdk" | "vdi" | "qcow2" | "iso" | "img" | "avhdx" | "vmrs"
            | "vmcx" | "vsv" | "bin_img" | "dmg" | "raw" => DiskImage,
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "ico"
            | "psd" | "ai" | "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "mp4"
            | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "mpg" | "mpeg" | "flv" | "3gp"
            | "ttf" | "otf" | "woff" | "woff2" | "eot" => Media,
            "exe" | "dll" | "sys" | "ocx" | "cpl" | "scr" | "drv" | "efi" | "mui" | "pdb"
            | "lib" | "obj" | "o" | "a" | "so" | "dylib" | "winmd" | "msi" | "msp" | "msu"
            | "pyc" | "pyd" | "class" | "wasm" | "node" | "com" | "ax" | "tlb" | "res" | "dmp"
            | "mdmp" | "hdmp" | "evtx" | "evt" | "etl_bin" | "pf" | "cat" | "nls" | "xex"
            | "ngen" | "ni" | "map_bin" => Binary,
            "db" | "sqlite" | "sqlite3" | "db3" | "s3db" | "mdb" | "accdb" | "mdf" | "ldf"
            | "ndf" | "bak" | "dbf" | "ibd" | "frm" | "myd" | "myi" | "rdb" | "aof" | "ldb"
            | "sst" | "idb" | "i64" | "id0" | "id1" | "nam" | "til" | "edb" | "sdb" | "dat_db"
            | "pst" | "ost" | "chk" | "jrs" => Database,
            _ => Unknown,
        }
    }
}

/// Lowercase extension without the leading dot. Returns `""` when there is no
/// extension. Names starting with a dot (`.gitignore`) are treated as
/// extensionless-with-hidden-name unless they contain a second dot.
pub fn extension_of(name: &str) -> String {
    let trimmed = name.trim_end_matches('.');
    match trimmed.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => {
            let ext = &trimmed[i + 1..];
            if ext.len() > 32 || ext.contains(char::is_whitespace) {
                String::new()
            } else {
                ext.to_ascii_lowercase()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension_of("README.MD"), "md");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of(".gitignore"), "");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of("weird."), "");
        assert_eq!(extension_of("a.this is not an ext"), "");
    }

    #[test]
    fn kinds() {
        assert_eq!(FileKind::from_extension("cs"), FileKind::Code);
        assert_eq!(FileKind::from_extension("vhdx"), FileKind::DiskImage);
        assert_eq!(FileKind::from_extension("zip"), FileKind::Archive);
        assert_eq!(FileKind::from_extension(""), FileKind::Unknown);
        assert!(FileKind::from_extension("log").is_text_candidate());
        assert!(!FileKind::from_extension("dll").is_text_candidate());
    }
}
