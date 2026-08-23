//! ZIP central-directory inventory.
//!
//! Reads only the end-of-central-directory record (plus its ZIP64 locator
//! and record when present) and the central directory itself, streaming
//! entry by entry under explicit budgets. Member data is never inflated; no
//! local header is read. Member names are normalised into a virtual path
//! namespace with traversal, absolute, and backslash forms flagged rather
//! than trusted.

use eidos_domain::archive::ArchiveFormat;
use eidos_domain::UnixNanos;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;

pub(crate) const SIG_CENTRAL: u32 = 0x0201_4b50;
pub(crate) const SIG_EOCD: u32 = 0x0605_4b50;
pub(crate) const SIG_EOCD64_LOCATOR: u32 = 0x0706_4b50;
pub(crate) const SIG_EOCD64: u32 = 0x0606_4b50;
const EOCD_LEN: usize = 22;
const EOCD64_LOCATOR_LEN: usize = 20;
const EOCD64_LEN: usize = 56;
const CENTRAL_LEN: usize = 46;
/// EOCD plus the largest possible comment plus the ZIP64 locator.
const TAIL_LEN: u64 = (EOCD_LEN + 0xFFFF + EOCD64_LOCATOR_LEN) as u64;

/// Budgets for one inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveLimits {
    /// Members read before the inventory is cut short (`truncated`).
    pub max_members: u64,
    /// Central-directory bytes read before the inventory is cut short.
    pub max_directory_bytes: u64,
    /// Longest member name accepted; longer names are a corruption sign.
    pub max_name_bytes: usize,
    /// Archive comment kept (bytes).
    pub max_comment_bytes: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_members: 1_000_000,
            max_directory_bytes: 256 * 1024 * 1024,
            max_name_bytes: 4096,
            max_comment_bytes: 1024,
        }
    }
}

/// Why a member name was not taken at face value (bit flags).
pub mod flag {
    /// Contained `..` segments (dropped from the virtual path).
    pub const TRAVERSAL: u32 = 1 << 0;
    /// Started with `/` or a drive letter (stripped).
    pub const ABSOLUTE: u32 = 1 << 1;
    /// Used `\` separators (normalised to `/`).
    pub const BACKSLASH: u32 = 1 << 2;
    /// Contained control characters.
    pub const CONTROL: u32 = 1 << 3;
    /// Another member already used the same virtual path.
    pub const DUPLICATE: u32 = 1 << 4;
    /// Name bytes were not valid UTF-8 (decoded as CP437 or lossily).
    pub const ENCODING: u32 = 1 << 5;
    /// Name was empty after normalisation.
    pub const EMPTY: u32 = 1 << 6;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Position in the central directory; implicit directories follow the
    /// explicit members.
    pub ordinal: u32,
    /// Name as stored, decoded (UTF-8 when flagged or valid, else CP437).
    pub raw_name: String,
    /// Normalised virtual path (`/`-separated, no leading `/`, no `.`/`..`
    /// segments, no trailing `/`).
    pub path: String,
    /// Last segment of `path`.
    pub name: String,
    /// Parent virtual path (`""` at the root).
    pub parent: String,
    pub is_dir: bool,
    /// Directory synthesised from member paths, not listed in the archive.
    pub implicit: bool,
    /// Declared uncompressed size.
    pub size: u64,
    pub compressed: u64,
    /// Compression method (0 stored, 8 deflate, 93 zstd, …).
    pub method: u16,
    pub crc32: u32,
    pub modified: Option<UnixNanos>,
    pub encrypted: bool,
    /// `flag::*` bits.
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub format: ArchiveFormat,
    pub members: Vec<Member>,
    /// Explicit members (files and directory entries) read.
    pub member_count: u64,
    /// Directories, explicit and implicit.
    pub dir_count: u64,
    pub implicit_dir_count: u64,
    /// Members whose names were flagged.
    pub suspicious_count: u64,
    /// Sum of declared uncompressed sizes.
    pub declared_size: u64,
    pub compressed_size: u64,
    /// Entries the end record claims.
    pub claimed_entries: u64,
    pub zip64: bool,
    /// A budget cut the inventory short.
    pub truncated: bool,
    pub truncated_reason: Option<String>,
    /// Archive comment (bounded), if any.
    pub comment: Option<String>,
    /// Bytes of container metadata read.
    pub bytes_read: u64,
    pub elapsed_ms: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// No end-of-central-directory record: not a ZIP file (or one with a
    /// truncated tail).
    #[error("no end-of-central-directory record")]
    NotZip,
    /// Structure inconsistent with the ZIP specification.
    #[error("corrupt archive: {0}")]
    Corrupt(String),
}

/// Inventory the members of the ZIP file at `path`.
pub fn inventory(path: &Path, limits: &ArchiveLimits) -> Result<Inventory, ArchiveError> {
    let file = open_shared(path)?;
    let len = file.metadata()?.len();
    inventory_reader(file, len, limits)
}

fn open_shared(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Never block writers or deleters; random access.
        o.share_mode(0x1 | 0x2 | 0x4);
        o.custom_flags(0x1000_0000);
    }
    o.open(path)
}

struct EndRecord {
    entries: u64,
    cd_size: u64,
    cd_offset: u64,
    /// Absolute offset where the directory must end (EOCD64 record or the
    /// locator/EOCD, whichever comes first).
    cd_end_limit: u64,
    zip64: bool,
    comment: Option<String>,
    bytes_read: u64,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

/// Locate and parse the end records. Reads at most `TAIL_LEN` bytes from the
/// end plus one 56-byte ZIP64 record.
fn read_end<R: Read + Seek>(
    r: &mut R,
    len: u64,
    limits: &ArchiveLimits,
) -> Result<EndRecord, ArchiveError> {
    if len < EOCD_LEN as u64 {
        return Err(ArchiveError::NotZip);
    }
    let tail_len = len.min(TAIL_LEN) as usize;
    let tail_start = len - tail_len as u64;
    r.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0u8; tail_len];
    r.read_exact(&mut tail)?;
    let mut bytes_read = tail_len as u64;

    // The EOCD is the last record whose comment length reaches the end of
    // the file exactly; scan backwards for a signature that satisfies it.
    let mut eocd_pos: Option<usize> = None;
    let mut p = tail_len - EOCD_LEN;
    loop {
        if u32_at(&tail, p) == SIG_EOCD {
            let comment_len = u16_at(&tail, p + 20) as usize;
            if p + EOCD_LEN + comment_len == tail_len {
                eocd_pos = Some(p);
                break;
            }
        }
        if p == 0 {
            break;
        }
        p -= 1;
    }
    let eocd = eocd_pos.ok_or(ArchiveError::NotZip)?;
    let eocd_abs = tail_start + eocd as u64;
    let disk = u16_at(&tail, eocd + 4);
    let cd_disk = u16_at(&tail, eocd + 6);
    let entries_disk = u16_at(&tail, eocd + 8) as u64;
    let entries = u16_at(&tail, eocd + 10) as u64;
    let cd_size = u32_at(&tail, eocd + 12) as u64;
    let cd_offset = u32_at(&tail, eocd + 16) as u64;
    let comment_len = u16_at(&tail, eocd + 20) as usize;
    let comment = (comment_len > 0).then(|| {
        let c = &tail[eocd + EOCD_LEN..eocd + EOCD_LEN + comment_len.min(limits.max_comment_bytes)];
        String::from_utf8_lossy(c).into_owned()
    });

    let needs_zip64 = disk == 0xFFFF
        || cd_disk == 0xFFFF
        || entries_disk == 0xFFFF
        || entries == 0xFFFF
        || cd_size == 0xFFFF_FFFF
        || cd_offset == 0xFFFF_FFFF;
    let has_locator = eocd >= EOCD64_LOCATOR_LEN
        && u32_at(&tail, eocd - EOCD64_LOCATOR_LEN) == SIG_EOCD64_LOCATOR;

    if !has_locator {
        if needs_zip64 {
            return Err(ArchiveError::Corrupt(
                "end record needs ZIP64 fields but no ZIP64 locator precedes it".into(),
            ));
        }
        if disk != 0 || cd_disk != 0 || entries_disk != entries {
            return Err(ArchiveError::Corrupt(
                "multi-volume archives are not supported".into(),
            ));
        }
        return Ok(EndRecord {
            entries,
            cd_size,
            cd_offset,
            cd_end_limit: eocd_abs,
            zip64: false,
            comment,
            bytes_read,
        });
    }

    // ZIP64: locator → record.
    let loc = eocd - EOCD64_LOCATOR_LEN;
    let loc_abs = tail_start + loc as u64;
    let eocd64_disk = u32_at(&tail, loc + 4);
    let eocd64_offset = u64_at(&tail, loc + 8);
    let total_disks = u32_at(&tail, loc + 16);
    if eocd64_disk != 0 || total_disks > 1 {
        return Err(ArchiveError::Corrupt(
            "multi-volume archives are not supported".into(),
        ));
    }
    if eocd64_offset.saturating_add(EOCD64_LEN as u64) > loc_abs {
        return Err(ArchiveError::Corrupt(
            "ZIP64 end record offset points past its locator".into(),
        ));
    }
    let mut rec = [0u8; EOCD64_LEN];
    r.seek(SeekFrom::Start(eocd64_offset))?;
    r.read_exact(&mut rec)?;
    bytes_read += EOCD64_LEN as u64;
    if u32_at(&rec, 0) != SIG_EOCD64 {
        return Err(ArchiveError::Corrupt(
            "ZIP64 end record signature missing".into(),
        ));
    }
    let disk64 = u32_at(&rec, 16);
    let cd_disk64 = u32_at(&rec, 20);
    let entries_disk64 = u64_at(&rec, 24);
    let entries64 = u64_at(&rec, 32);
    let cd_size64 = u64_at(&rec, 40);
    let cd_offset64 = u64_at(&rec, 48);
    if disk64 != 0 || cd_disk64 != 0 || entries_disk64 != entries64 {
        return Err(ArchiveError::Corrupt(
            "multi-volume archives are not supported".into(),
        ));
    }
    Ok(EndRecord {
        entries: entries64,
        cd_size: cd_size64,
        cd_offset: cd_offset64,
        cd_end_limit: eocd64_offset,
        zip64: true,
        comment,
        bytes_read,
    })
}

/// DOS date/time (local, 2-second resolution) to Unix nanoseconds, treated
/// as UTC because the archive records no zone.
pub fn dos_datetime(date: u16, time: u16) -> Option<UnixNanos> {
    let year = 1980 + (date >> 9) as i64;
    let month = ((date >> 5) & 0xF) as i64;
    let day = (date & 0x1F) as i64;
    let hour = (time >> 11) as i64;
    let minute = ((time >> 5) & 0x3F) as i64;
    let second = ((time & 0x1F) as i64) * 2;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some(UnixNanos(secs.checked_mul(1_000_000_000)?))
}

/// CP437 (the original PKZIP code page) for the high half.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

fn decode_name(bytes: &[u8], utf8_flag: bool, unicode_extra: Option<&[u8]>) -> (String, bool) {
    if let Some(u) = unicode_extra {
        if let Ok(s) = std::str::from_utf8(u) {
            return (s.to_string(), false);
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) if utf8_flag => (String::from_utf8_lossy(bytes).into_owned(), true),
        Err(_) => (
            bytes
                .iter()
                .map(|&b| {
                    if b < 0x80 {
                        b as char
                    } else {
                        CP437_HIGH[(b - 0x80) as usize]
                    }
                })
                .collect(),
            true,
        ),
    }
}

/// Normalise a stored name into a virtual path; returns (path, flags).
pub fn normalize_name(raw: &str) -> (String, u32) {
    let mut flags = 0u32;
    let mut s = raw.to_string();
    if s.contains('\\') {
        flags |= flag::BACKSLASH;
        s = s.replace('\\', "/");
    }
    if s.chars().any(|c| c.is_control()) {
        flags |= flag::CONTROL;
        s = s.chars().filter(|c| !c.is_control()).collect();
    }
    // Drive letter or UNC prefix.
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        flags |= flag::ABSOLUTE;
        s = s[2..].to_string();
    }
    if s.starts_with('/') {
        flags |= flag::ABSOLUTE;
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        match seg {
            "" | "." => {}
            ".." => flags |= flag::TRAVERSAL,
            other => parts.push(other),
        }
    }
    let path = parts.join("/");
    if path.is_empty() {
        flags |= flag::EMPTY;
    }
    (path, flags)
}

struct Extra {
    zip64_sizes: Option<(Option<u64>, Option<u64>)>,
    unicode_name: Option<Vec<u8>>,
    mtime: Option<UnixNanos>,
}

fn parse_extra(extra: &[u8], size_ff: bool, comp_ff: bool) -> Extra {
    let mut out = Extra {
        zip64_sizes: None,
        unicode_name: None,
        mtime: None,
    };
    let mut i = 0usize;
    while i + 4 <= extra.len() {
        let id = u16_at(extra, i);
        let n = u16_at(extra, i + 2) as usize;
        let data = match extra.get(i + 4..i + 4 + n) {
            Some(d) => d,
            None => break,
        };
        match id {
            0x0001 => {
                // Fields appear only for the 32-bit values that were 0xFFFFFFFF,
                // in the order uncompressed, compressed, local offset, disk.
                let mut j = 0usize;
                let mut size = None;
                let mut comp = None;
                if size_ff && j + 8 <= data.len() {
                    size = Some(u64_at(data, j));
                    j += 8;
                }
                if comp_ff && j + 8 <= data.len() {
                    comp = Some(u64_at(data, j));
                }
                out.zip64_sizes = Some((size, comp));
            }
            0x7075 => {
                // Info-ZIP Unicode Path: version, CRC of the stored name, UTF-8 name.
                if data.len() > 5 && data[0] == 1 {
                    out.unicode_name = Some(data[5..].to_vec());
                }
            }
            0x5455 => {
                // Extended timestamp: flags byte, then mtime when bit 0 is set.
                if data.len() >= 5 && data[0] & 1 == 1 {
                    let t = u32_at(data, 1) as i64;
                    out.mtime = Some(UnixNanos(t * 1_000_000_000));
                }
            }
            0x000a => {
                // NTFS: reserved u32, then attribute records (tag, size, data).
                let mut k = 4usize;
                while k + 4 <= data.len() {
                    let tag = u16_at(data, k);
                    let sz = u16_at(data, k + 2) as usize;
                    if tag == 1 && sz >= 24 && k + 4 + 8 <= data.len() {
                        let ticks = u64_at(data, k + 4) as i64;
                        out.mtime = Some(UnixNanos::from_filetime_ticks(ticks));
                    }
                    k += 4 + sz;
                }
            }
            _ => {}
        }
        i += 4 + n;
    }
    out
}

/// Inventory from any seekable reader of `len` bytes.
pub fn inventory_reader<R: Read + Seek>(
    mut r: R,
    len: u64,
    limits: &ArchiveLimits,
) -> Result<Inventory, ArchiveError> {
    let started = Instant::now();
    let end = read_end(&mut r, len, limits)?;
    if end.cd_offset.saturating_add(end.cd_size) > end.cd_end_limit {
        return Err(ArchiveError::Corrupt(format!(
            "central directory ({} bytes at {}) extends past its end record at {}",
            end.cd_size, end.cd_offset, end.cd_end_limit
        )));
    }
    let mut inv = Inventory {
        format: ArchiveFormat::Zip,
        members: Vec::new(),
        member_count: 0,
        dir_count: 0,
        implicit_dir_count: 0,
        suspicious_count: 0,
        declared_size: 0,
        compressed_size: 0,
        claimed_entries: end.entries,
        zip64: end.zip64,
        truncated: false,
        truncated_reason: None,
        comment: end.comment,
        bytes_read: end.bytes_read,
        elapsed_ms: 0.0,
    };

    r.seek(SeekFrom::Start(end.cd_offset))?;
    let budget = end.cd_size.min(limits.max_directory_bytes);
    let mut cd = BufReader::with_capacity(256 * 1024, r.take(budget));
    let mut consumed = 0u64;
    let mut header = [0u8; CENTRAL_LEN];
    let mut seen: HashSet<String> = HashSet::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut explicit_dirs: HashSet<String> = HashSet::new();

    while inv.member_count < end.entries {
        if inv.member_count >= limits.max_members {
            inv.truncated = true;
            inv.truncated_reason = Some(format!(
                "member limit ({}) reached; the archive claims {} entries",
                limits.max_members, end.entries
            ));
            break;
        }
        if consumed + CENTRAL_LEN as u64 > budget {
            if budget < end.cd_size {
                inv.truncated = true;
                inv.truncated_reason = Some(format!(
                    "central-directory byte limit ({}) reached; the directory is {} bytes",
                    limits.max_directory_bytes, end.cd_size
                ));
                break;
            }
            return Err(ArchiveError::Corrupt(format!(
                "central directory ends after {} of {} claimed entries",
                inv.member_count, end.entries
            )));
        }
        cd.read_exact(&mut header)?;
        consumed += CENTRAL_LEN as u64;
        let sig = u32_at(&header, 0);
        if sig != SIG_CENTRAL {
            return Err(ArchiveError::Corrupt(format!(
                "entry {} has signature {sig:#010x}, expected a central file header",
                inv.member_count
            )));
        }
        let gp_flags = u16_at(&header, 8);
        let method = u16_at(&header, 10);
        let dos_time = u16_at(&header, 12);
        let dos_date = u16_at(&header, 14);
        let crc32 = u32_at(&header, 16);
        let comp32 = u32_at(&header, 20);
        let size32 = u32_at(&header, 24);
        let name_len = u16_at(&header, 28) as usize;
        let extra_len = u16_at(&header, 30) as usize;
        let comment_len = u16_at(&header, 32) as usize;
        let external = u32_at(&header, 38);
        if name_len > limits.max_name_bytes {
            return Err(ArchiveError::Corrupt(format!(
                "entry {} name is {name_len} bytes",
                inv.member_count
            )));
        }
        let var_len = (name_len + extra_len + comment_len) as u64;
        if consumed + var_len > budget {
            if budget < end.cd_size {
                inv.truncated = true;
                inv.truncated_reason = Some("central-directory byte limit reached".into());
                break;
            }
            return Err(ArchiveError::Corrupt(format!(
                "entry {} runs past the end of the central directory",
                inv.member_count
            )));
        }
        let mut var = vec![0u8; var_len as usize];
        cd.read_exact(&mut var)?;
        consumed += var_len;
        let name_bytes = &var[..name_len];
        let extra_bytes = &var[name_len..name_len + extra_len];

        let size_ff = size32 == 0xFFFF_FFFF;
        let comp_ff = comp32 == 0xFFFF_FFFF;
        let extra = parse_extra(extra_bytes, size_ff, comp_ff);
        let (mut size, mut compressed) = (size32 as u64, comp32 as u64);
        if let Some((s, c)) = extra.zip64_sizes {
            if let Some(s) = s {
                size = s;
            }
            if let Some(c) = c {
                compressed = c;
            }
        }
        let (raw_name, bad_encoding) = decode_name(
            name_bytes,
            gp_flags & 0x0800 != 0,
            extra.unicode_name.as_deref(),
        );
        let (path, mut flags) = normalize_name(&raw_name);
        if bad_encoding {
            flags |= flag::ENCODING;
        }
        let is_dir = raw_name.ends_with('/')
            || raw_name.ends_with('\\')
            || (external & 0x10 != 0 && size == 0 && !path.is_empty() && !raw_name.contains('.'));
        let path = if path.is_empty() {
            format!("(unnamed member {})", inv.member_count)
        } else {
            path
        };
        if !seen.insert(path.clone()) {
            flags |= flag::DUPLICATE;
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), path.clone()),
        };
        // Every ancestor is a directory of the virtual tree.
        let mut anc = parent.as_str();
        while !anc.is_empty() {
            dirs.insert(anc.to_string());
            anc = anc.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        }
        if is_dir {
            explicit_dirs.insert(path.clone());
            dirs.insert(path.clone());
        } else {
            inv.declared_size = inv.declared_size.saturating_add(size);
            inv.compressed_size = inv.compressed_size.saturating_add(compressed);
        }
        if flags != 0 {
            inv.suspicious_count += 1;
        }
        let modified = extra.mtime.or_else(|| dos_datetime(dos_date, dos_time));
        inv.members.push(Member {
            ordinal: inv.member_count as u32,
            raw_name,
            path,
            name,
            parent,
            is_dir,
            implicit: false,
            size: if is_dir { 0 } else { size },
            compressed: if is_dir { 0 } else { compressed },
            method,
            crc32,
            modified,
            encrypted: gp_flags & 1 != 0,
            flags,
        });
        inv.member_count += 1;
    }
    inv.bytes_read += consumed;

    // Implicit directories: ancestors no member listed.
    let mut next = inv.member_count as u32;
    for d in dirs {
        if explicit_dirs.contains(&d) {
            continue;
        }
        let (parent, name) = match d.rsplit_once('/') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), d.clone()),
        };
        inv.members.push(Member {
            ordinal: next,
            raw_name: format!("{d}/"),
            path: d,
            name,
            parent,
            is_dir: true,
            implicit: true,
            size: 0,
            compressed: 0,
            method: 0,
            crc32: 0,
            modified: None,
            encrypted: false,
            flags: 0,
        });
        next += 1;
        inv.implicit_dir_count += 1;
    }
    inv.dir_count = explicit_dirs.len() as u64 + inv.implicit_dir_count;
    inv.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(inv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{build, Entry};
    use std::io::Cursor;

    fn inv(bytes: &[u8]) -> Result<Inventory, ArchiveError> {
        inventory_reader(
            Cursor::new(bytes.to_vec()),
            bytes.len() as u64,
            &ArchiveLimits::default(),
        )
    }

    #[test]
    fn members_topology_and_sizes() {
        let z = build(
            &[
                Entry::file("readme.txt", b"hello"),
                Entry::dir("src/"),
                Entry::file("src/lib/mod.rs", b"fn x() {}"),
                Entry::file("assets/logo.png", &[0u8; 300]),
            ],
            b"a comment",
            false,
        );
        let i = inv(&z).unwrap();
        assert_eq!(i.member_count, 4);
        assert_eq!(i.claimed_entries, 4);
        assert_eq!(i.declared_size, 5 + 9 + 300);
        assert_eq!(i.comment.as_deref(), Some("a comment"));
        assert!(!i.zip64 && !i.truncated);
        let paths: Vec<(&str, bool, bool)> = i
            .members
            .iter()
            .map(|m| (m.path.as_str(), m.is_dir, m.implicit))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("readme.txt", false, false),
                ("src", true, false),
                ("src/lib/mod.rs", false, false),
                ("assets/logo.png", false, false),
                ("assets", true, true),
                ("src/lib", true, true),
            ]
        );
        assert_eq!(i.dir_count, 3);
        assert_eq!(i.implicit_dir_count, 2);
        let m = &i.members[2];
        assert_eq!((m.parent.as_str(), m.name.as_str()), ("src/lib", "mod.rs"));
        assert_eq!(m.crc32, 0xDEAD_BEEF);
        // 2026-08-21 12:00:00 UTC
        assert_eq!(m.modified, Some(UnixNanos(1_787_313_600 * 1_000_000_000)));
        assert_eq!(i.suspicious_count, 0);
    }

    #[test]
    fn names_are_normalised_and_flagged() {
        let z = build(
            &[
                Entry::file("../../etc/passwd", b"x"),
                Entry::file("dir\\sub\\file.txt", b"x"),
                Entry::file("/abs/path.txt", b"x"),
                Entry::file("C:\\win\\path.txt", b"x"),
                Entry::file("dup.txt", b"x"),
                Entry::file("./dup.txt", b"x"),
                Entry::file("", b"x"),
            ],
            b"",
            false,
        );
        let i = inv(&z).unwrap();
        let m = &i.members;
        assert_eq!(m[0].path, "etc/passwd");
        assert!(m[0].flags & flag::TRAVERSAL != 0);
        assert_eq!(m[1].path, "dir/sub/file.txt");
        assert!(m[1].flags & flag::BACKSLASH != 0);
        assert_eq!(m[2].path, "abs/path.txt");
        assert!(m[2].flags & flag::ABSOLUTE != 0);
        assert_eq!(m[3].path, "win/path.txt");
        assert_eq!(m[3].flags, flag::ABSOLUTE | flag::BACKSLASH);
        assert_eq!(m[4].flags, 0);
        assert!(m[5].flags & flag::DUPLICATE != 0);
        assert!(m[6].flags & flag::EMPTY != 0);
        assert!(m[6].path.starts_with("(unnamed member"));
        assert_eq!(i.suspicious_count, 6);
    }

    #[test]
    fn name_encodings() {
        let mut cp437 = Entry::file("", b"x");
        cp437.name = b"caf\x82.txt".to_vec(); // é in CP437
        let mut utf8 = Entry::file("naïve.txt", b"x");
        utf8.flags = 0x0800;
        let mut unicode_extra = Entry::file("", b"x");
        unicode_extra.name = b"legacy.txt".to_vec();
        let mut extra = vec![0x75, 0x70];
        let payload = [&[1u8][..], &[0, 0, 0, 0], "ünïcode.txt".as_bytes()].concat();
        extra.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        extra.extend_from_slice(&payload);
        unicode_extra.extra = extra;
        let z = build(&[cp437, utf8, unicode_extra], b"", false);
        let i = inv(&z).unwrap();
        assert_eq!(i.members[0].path, "café.txt");
        assert!(i.members[0].flags & flag::ENCODING != 0);
        assert_eq!(i.members[1].path, "naïve.txt");
        assert_eq!(i.members[1].flags, 0);
        assert_eq!(i.members[2].path, "ünïcode.txt");
        assert_eq!(i.members[2].raw_name, "ünïcode.txt");
    }

    #[test]
    fn zip64_end_records_and_member_sizes() {
        let mut big = Entry::file("huge.bin", b"");
        big.size_override = Some((0xFFFF_FFFF, 0xFFFF_FFFF));
        let mut extra = vec![0x01, 0x00, 16, 0];
        extra.extend_from_slice(&(5u64 << 32).to_le_bytes());
        extra.extend_from_slice(&(3u64 << 32).to_le_bytes());
        big.extra = extra;
        let z = build(&[big, Entry::file("small.txt", b"abc")], b"", true);
        let i = inv(&z).unwrap();
        assert!(i.zip64);
        assert_eq!(i.member_count, 2);
        assert_eq!(i.members[0].size, 5u64 << 32);
        assert_eq!(i.members[0].compressed, 3u64 << 32);
        assert_eq!(i.declared_size, (5u64 << 32) + 3);
    }

    #[test]
    fn corrupt_and_truncated_inputs_fail_cleanly() {
        let z = build(&[Entry::file("a.txt", b"x")], b"", false);
        // Truncated tail: no end record.
        assert!(matches!(inv(&z[..z.len() - 5]), Err(ArchiveError::NotZip)));
        // Random bytes.
        assert!(matches!(inv(&[7u8; 100]), Err(ArchiveError::NotZip)));
        assert!(matches!(inv(b"PK"), Err(ArchiveError::NotZip)));
        // Central directory offset past the end record.
        let mut bad = z.clone();
        let n = bad.len();
        bad[n - 6..n - 2].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
        assert!(matches!(inv(&bad), Err(ArchiveError::Corrupt(_))));
        // Claims far more entries than the directory holds.
        let mut bomb = z.clone();
        bomb[n - 12..n - 10].copy_from_slice(&60_000u16.to_le_bytes());
        bomb[n - 14..n - 12].copy_from_slice(&60_000u16.to_le_bytes());
        match inv(&bomb) {
            Err(ArchiveError::Corrupt(m)) => assert!(m.contains("ends after 1 of 60000"), "{m}"),
            other => panic!("{other:?}"),
        }
        // Entry signature garbage inside the directory.
        let mut garbage = z.clone();
        let cd = garbage.len() - EOCD_LEN - (CENTRAL_LEN + 5);
        garbage[cd] = 0;
        assert!(matches!(inv(&garbage), Err(ArchiveError::Corrupt(_))));
        // ZIP64 locator pointing past the file.
        let z64 = build(&[Entry::file("a.txt", b"x")], b"", true);
        let mut bad64 = z64.clone();
        let loc = bad64.len() - EOCD_LEN - EOCD64_LOCATOR_LEN;
        bad64[loc + 8..loc + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(inv(&bad64), Err(ArchiveError::Corrupt(_))));
    }

    #[test]
    fn budgets_truncate_instead_of_failing() {
        let entries: Vec<Entry> = (0..10)
            .map(|i| Entry::file(&format!("f{i}.txt"), b"x"))
            .collect();
        let z = build(&entries, b"", false);
        let limits = ArchiveLimits {
            max_members: 3,
            ..ArchiveLimits::default()
        };
        let i = inventory_reader(Cursor::new(z.clone()), z.len() as u64, &limits).unwrap();
        assert!(i.truncated);
        assert_eq!(i.member_count, 3);
        assert_eq!(i.claimed_entries, 10);
        let limits = ArchiveLimits {
            max_directory_bytes: (CENTRAL_LEN + 6) as u64 * 2 + 3,
            ..ArchiveLimits::default()
        };
        let i = inventory_reader(Cursor::new(z.clone()), z.len() as u64, &limits).unwrap();
        assert!(i.truncated);
        assert_eq!(i.member_count, 2);
    }

    #[test]
    fn comment_scan_is_not_fooled_by_signature_bytes() {
        // The comment itself contains an EOCD signature; the strict
        // comment-length check must skip it.
        let mut comment = SIG_EOCD.to_le_bytes().to_vec();
        comment.extend_from_slice(&[0xFFu8; 18]);
        let z = build(&[Entry::file("a.txt", b"x")], &comment, false);
        let i = inv(&z).unwrap();
        assert_eq!(i.member_count, 1);
    }

    #[test]
    fn dos_time_conversion() {
        assert_eq!(
            dos_datetime(0x0021, 0),
            Some(UnixNanos(315_532_800 * 1_000_000_000))
        ); // 1980-01-01
        assert_eq!(dos_datetime(0x0000, 0), None); // month 0
        assert_eq!(
            dos_datetime(0x5D15, 0x6000),
            Some(UnixNanos(1_787_313_600 * 1_000_000_000))
        );
    }
}
