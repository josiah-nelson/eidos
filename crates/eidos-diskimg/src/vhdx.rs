//! VHDX container parsing: headers, region table, metadata, and the block
//! allocation table (BAT), exposed as a `Read + Seek` view of the virtual
//! disk.
//!
//! Every field read here comes from an untrusted file, so every declared
//! size, count, and offset is range-checked before it is used to allocate or
//! to seek. Fixed and dynamic payloads share one code path and use one of the
//! specification-permitted zero-return behaviours for undefined contents.
//! Payload reads are refused while log replay is pending: a read-only parser
//! does not implement the required in-memory replay, and the on-disk BAT or
//! payload may not yet represent the current virtual disk. Differencing disks are recognised from
//! the file-parameters flags and their parent locator is decoded, but their BAT
//! is never interpreted: without the parent image the payload blocks are
//! meaningless.
//!
//! Reference: Microsoft [MS-VHDX], "Virtual Hard Disk v2 File Format".

use crate::{DiskImageError, DiskImageLimits};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::OnceLock;

const MB: u64 = 1024 * 1024;
const FILE_SIGNATURE: &[u8; 8] = b"vhdxfile";
const HEADER_SIGNATURE: &[u8; 4] = b"head";
const REGION_SIGNATURE: &[u8; 4] = b"regi";
const METADATA_SIGNATURE: &[u8; 8] = b"metadata";
const HEADER_OFFSETS: [u64; 2] = [0x1_0000, 0x2_0000];
const REGION_TABLE_OFFSETS: [u64; 2] = [0x3_0000, 0x4_0000];
const HEADER_LEN: usize = 4096;
const REGION_TABLE_LEN: usize = 64 * 1024;
const REGION_ENTRY_LEN: usize = 32;
const METADATA_TABLE_HEADER_LEN: usize = 32;
const METADATA_ENTRY_LEN: usize = 32;
const SPEC_MAX_REGION_ENTRIES: u32 = 2047;
const SPEC_MAX_METADATA_ENTRIES: u16 = 2047;
const SPEC_MAX_VIRTUAL_SIZE: u64 = 64 << 40;
/// Metadata item payloads must live within this much of the region start.
const METADATA_SPAN: u64 = MB;
/// Smallest file that can hold the headers and both region tables.
const MIN_FILE_LEN: u64 = MB;

// GUIDs are stored the way Windows serialises them: the first three fields
// little-endian, the last eight bytes in order.
const BAT_REGION: [u8; 16] = *b"\x66\x77\xC2\x2D\x23\xF6\x00\x42\x9D\x64\x11\x5E\x9B\xFD\x4A\x08";
const METADATA_REGION: [u8; 16] =
    *b"\x06\xA2\x7C\x8B\x90\x47\x9A\x4B\xB8\xFE\x57\x5F\x05\x0F\x88\x6E";
const FILE_PARAMETERS: [u8; 16] =
    *b"\x37\x67\xA1\xCA\x36\xFA\x43\x4D\xB3\xB6\x33\xF0\xAA\x44\xE7\x6B";
const VIRTUAL_DISK_SIZE: [u8; 16] =
    *b"\x24\x42\xA5\x2F\x1B\xCD\x76\x48\xB2\x11\x5D\xBE\xD8\x3B\xF4\xB8";
const LOGICAL_SECTOR_SIZE: [u8; 16] =
    *b"\x1D\xBF\x41\x81\x6F\xA9\x09\x47\xBA\x47\xF2\x33\xA8\xFA\xAB\x5F";
const PHYSICAL_SECTOR_SIZE: [u8; 16] =
    *b"\xC7\x48\xA3\xCD\x5D\x44\x71\x44\x9C\xC9\xE9\x88\x52\x51\xC5\x56";
const PARENT_LOCATOR: [u8; 16] =
    *b"\x2D\x5F\xD3\xA8\x0B\xB3\x4D\x45\xAB\xF7\xD3\xD8\x48\x34\xAB\x0C";
const VHDX_PARENT_LOCATOR_TYPE: [u8; 16] =
    *b"\xB7\xEF\x4A\xB0\x9E\xD1\x81\x4A\xB7\x89\x25\xB8\xE9\x44\x59\x13";
/// The required GUID that identifies the virtual disk.
const VIRTUAL_DISK_ID: [u8; 16] =
    *b"\xAB\x12\xCA\xBE\xE6\xB2\x23\x45\x93\xEF\xC3\x09\xE0\x00\xC7\x46";

/// BAT entry states (the low three bits of an entry).
const PAYLOAD_BLOCK_FULLY_PRESENT: u64 = 6;
const PAYLOAD_BLOCK_PARTIALLY_PRESENT: u64 = 7;

/// How the payload of a VHDX is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    /// Every block is allocated up front (`LeaveBlocksAllocated`).
    Fixed,
    /// Blocks are allocated on first write; unallocated blocks read as zero.
    Dynamic,
    /// Blocks not present here come from a parent image (`HasParent`).
    Differencing,
}

/// The parent image a differencing disk was branched from, as the container
/// records it. Keys are specification-defined (`relative_path`,
/// `volume_path`, `absolute_win32_path`, `parent_linkage`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentLocator {
    /// GUID of the locator type; `b04aefb7-d19e-4a81-b789-25b8e9445913` is
    /// the only one Microsoft defines (a VHDX parent).
    pub locator_type: String,
    /// Key/value pairs, each bounded by
    /// [`DiskImageLimits::max_locator_bytes`].
    pub entries: Vec<(String, String)>,
}

impl ParentLocator {
    /// The first value stored under `key`, if any.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Geometry and identity of a VHDX, as its own metadata declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VhdxInfo {
    /// Free-form creator string from the file identifier.
    pub creator: String,
    /// Stable virtual-disk identity from the required metadata GUID.
    pub virtual_disk_id: String,
    pub payload: PayloadKind,
    pub virtual_size: u64,
    pub block_size: u32,
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
    /// Payload BAT entries between consecutive sector-bitmap entries.
    pub chunk_ratio: u64,
    /// Total required BAT slots, including interleaved sector-bitmap slots.
    pub bat_entries: u64,
    /// Payload blocks actually backed by bytes in this file.
    pub present_blocks: u64,
    /// Sequence number of the header that won.
    pub sequence_number: u64,
    /// The log holds entries a writer never flushed. This reader does not
    /// implement in-memory log replay, so payload reads are refused and the
    /// top-level inventory returns only container metadata.
    pub log_replay_pending: bool,
    pub parent: Option<ParentLocator>,
}

/// A `Read + Seek` view of the virtual disk described by a VHDX file.
pub struct VhdxDisk<R> {
    inner: R,
    info: VhdxInfo,
    /// Payload BAT entries only; sector-bitmap entries are dropped.
    bat: Vec<u64>,
    file_len: u64,
    pos: u64,
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
fn guid_at(b: &[u8], i: usize) -> [u8; 16] {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[i..i + 16]);
    a
}

/// Render a Windows-serialised GUID in the canonical hyphenated form.
pub fn guid_string(g: &[u8; 16]) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
        u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// CRC-32C (Castagnoli), which VHDX uses for its header and region-table
/// checksums — not the IEEE polynomial that `crc32fast` implements.
pub fn crc32c(bytes: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0x82F6_3B78
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in bytes {
        crc = (crc >> 8) ^ table[((crc ^ b as u32) & 0xFF) as usize];
    }
    !crc
}

/// UTF-16LE with a possible NUL terminator, decoded lossily and trimmed.
fn utf16_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn read_at<R: Read + Seek>(r: &mut R, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

struct Header {
    sequence_number: u64,
    log_guid: [u8; 16],
    log_version: u16,
    version: u16,
    log_length: u64,
    log_offset: u64,
    reserved_nonzero: bool,
}

/// Parse one 4 KiB header copy. Per MS-VHDX, signature and checksum alone
/// determine validity/currentness; the winning header's version and log
/// geometry are validated afterwards.
fn parse_header(buf: &[u8]) -> Option<Header> {
    if &buf[..4] != HEADER_SIGNATURE {
        return None;
    }
    let stored = u32_at(buf, 4);
    let mut zeroed = buf[..HEADER_LEN].to_vec();
    zeroed[4..8].fill(0);
    if crc32c(&zeroed) != stored {
        return None;
    }
    Some(Header {
        sequence_number: u64_at(buf, 8),
        log_guid: guid_at(buf, 48),
        log_version: u16_at(buf, 64),
        version: u16_at(buf, 66),
        log_length: u64::from(u32_at(buf, 68)),
        log_offset: u64_at(buf, 72),
        reserved_nonzero: buf[80..].iter().any(|byte| *byte != 0),
    })
}

#[derive(Clone, Copy)]
struct RegionEntry {
    offset: u64,
    length: u64,
}

struct RegionTable {
    bat: Option<RegionEntry>,
    metadata: Option<RegionEntry>,
    entries: Vec<RegionEntry>,
}

/// Parse one 64 KiB region table, returning the BAT and metadata regions.
fn parse_region_table(
    buf: &[u8],
    file_len: u64,
    limits: &DiskImageLimits,
) -> Result<RegionTable, DiskImageError> {
    if &buf[..4] != REGION_SIGNATURE {
        return Err(DiskImageError::Corrupt("region table signature".into()));
    }
    let stored = u32_at(buf, 4);
    let mut zeroed = buf[..REGION_TABLE_LEN].to_vec();
    zeroed[4..8].fill(0);
    if crc32c(&zeroed) != stored {
        return Err(DiskImageError::Corrupt("region table checksum".into()));
    }
    let count = u32_at(buf, 8);
    if count > SPEC_MAX_REGION_ENTRIES {
        return Err(DiskImageError::Corrupt(format!(
            "region table claims {count} entries, above the VHDX maximum of {SPEC_MAX_REGION_ENTRIES}"
        )));
    }
    if count > limits.max_region_entries {
        return Err(DiskImageError::Unsupported(format!(
            "region table needs {count} entries, above the configured limit of {}",
            limits.max_region_entries
        )));
    }
    if u32_at(buf, 12) != 0 {
        return Err(DiskImageError::Corrupt(
            "region table reserved field is non-zero".into(),
        ));
    }
    let mut bat = None;
    let mut metadata = None;
    let mut entries: Vec<RegionEntry> = Vec::with_capacity(count as usize);
    let mut guids = HashSet::with_capacity(count as usize);
    for i in 0..count as usize {
        let at = 16 + i * REGION_ENTRY_LEN;
        if at + REGION_ENTRY_LEN > REGION_TABLE_LEN {
            return Err(DiskImageError::Corrupt(format!(
                "region entry {i} runs past the 64 KiB region table"
            )));
        }
        let guid = guid_at(buf, at);
        if !guids.insert(guid) {
            return Err(DiskImageError::Corrupt(format!(
                "duplicate region {}",
                guid_string(&guid)
            )));
        }
        let offset = u64_at(buf, at + 16);
        let length = u32_at(buf, at + 24) as u64;
        let required_value = u32_at(buf, at + 28);
        if required_value > 1 {
            return Err(DiskImageError::Corrupt(format!(
                "region entry {i} has invalid required value {required_value}"
            )));
        }
        let required = required_value == 1;
        if matches!(guid, BAT_REGION | METADATA_REGION) && !required {
            return Err(DiskImageError::Corrupt(format!(
                "region {} must be marked required",
                guid_string(&guid)
            )));
        }
        // Regions are 1 MiB-aligned, 1 MiB-granular, and live past the
        // header area; anything else would let a crafted table point the
        // reader at the headers or outside the file.
        if offset < MB || offset % MB != 0 || length == 0 || length % MB != 0 {
            return Err(DiskImageError::Corrupt(format!(
                "region entry {i} is {length} bytes at {offset}, not 1 MiB-aligned past the headers"
            )));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| DiskImageError::Corrupt(format!("region entry {i} range overflows")))?;
        if end > file_len {
            return Err(DiskImageError::Corrupt(format!(
                "region entry {i} ends at {end} but the file is {file_len} bytes",
            )));
        }
        let entry = RegionEntry { offset, length };
        if entries
            .iter()
            .any(|other| offset < other.offset + other.length && other.offset < end)
        {
            return Err(DiskImageError::Corrupt(format!(
                "region entry {i} overlaps another region"
            )));
        }
        entries.push(entry);
        match guid {
            BAT_REGION => bat = Some(entry),
            METADATA_REGION => metadata = Some(entry),
            _ if required => {
                return Err(DiskImageError::Unsupported(format!(
                    "region {} is marked required but unknown",
                    guid_string(&guid)
                )))
            }
            _ => {}
        }
    }
    Ok(RegionTable {
        bat,
        metadata,
        entries,
    })
}

#[derive(Default)]
struct Metadata {
    block_size: Option<u32>,
    leave_blocks_allocated: bool,
    has_parent: bool,
    virtual_size: Option<u64>,
    logical_sector_size: Option<u32>,
    physical_sector_size: Option<u32>,
    parent: Option<ParentLocator>,
    virtual_disk_id: Option<[u8; 16]>,
}

fn parse_metadata(buf: &[u8], limits: &DiskImageLimits) -> Result<Metadata, DiskImageError> {
    if buf.len() < METADATA_TABLE_HEADER_LEN || &buf[..8] != METADATA_SIGNATURE {
        return Err(DiskImageError::Corrupt("metadata table signature".into()));
    }
    let count = u16_at(buf, 10);
    if count > SPEC_MAX_METADATA_ENTRIES {
        return Err(DiskImageError::Corrupt(format!(
            "metadata table claims {count} entries, above the VHDX maximum of {SPEC_MAX_METADATA_ENTRIES}"
        )));
    }
    if count > limits.max_metadata_entries {
        return Err(DiskImageError::Unsupported(format!(
            "metadata table needs {count} entries, above the configured limit of {}",
            limits.max_metadata_entries
        )));
    }
    if buf[8..10] != [0, 0]
        || buf[12..METADATA_TABLE_HEADER_LEN]
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err(DiskImageError::Corrupt(
            "metadata table reserved fields are non-zero".into(),
        ));
    }
    let mut md = Metadata::default();
    let mut seen = HashSet::new();
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(count as usize);
    let mut user_entries = 0usize;
    for i in 0..count as usize {
        let at = METADATA_TABLE_HEADER_LEN + i * METADATA_ENTRY_LEN;
        if at + METADATA_ENTRY_LEN > buf.len() {
            return Err(DiskImageError::Corrupt(format!(
                "metadata entry {i} runs past the metadata region"
            )));
        }
        let guid = guid_at(buf, at);
        let flags = u32_at(buf, at + 24);
        if flags & !0b111 != 0 || u32_at(buf, at + 28) != 0 {
            return Err(DiskImageError::Corrupt(format!(
                "metadata entry {i} has non-zero reserved fields"
            )));
        }
        let is_user = flags & 1 != 0;
        user_entries += usize::from(is_user);
        if user_entries > 1024 {
            return Err(DiskImageError::Corrupt(
                "metadata table contains more than 1024 user entries".into(),
            ));
        }
        if !seen.insert((guid, is_user)) {
            return Err(DiskImageError::Corrupt(format!(
                "duplicate metadata item {}",
                guid_string(&guid)
            )));
        }
        let offset = u32_at(buf, at + 16) as usize;
        let length = u32_at(buf, at + 20) as usize;
        let required = flags & 0b100 != 0;
        let expected_virtual = match guid {
            FILE_PARAMETERS | PARENT_LOCATOR => Some(false),
            VIRTUAL_DISK_SIZE | LOGICAL_SECTOR_SIZE | PHYSICAL_SECTOR_SIZE | VIRTUAL_DISK_ID => {
                Some(true)
            }
            _ => None,
        };
        if let Some(expected_virtual) = expected_virtual {
            let is_virtual = flags & 0b010 != 0;
            if !required || is_virtual != expected_virtual {
                return Err(DiskImageError::Corrupt(format!(
                    "metadata item {} has incorrect required/virtual-disk flags",
                    guid_string(&guid)
                )));
            }
        }
        if is_user
            && matches!(
                guid,
                FILE_PARAMETERS
                    | VIRTUAL_DISK_SIZE
                    | LOGICAL_SECTOR_SIZE
                    | PHYSICAL_SECTOR_SIZE
                    | PARENT_LOCATOR
                    | VIRTUAL_DISK_ID
            )
        {
            return Err(DiskImageError::Corrupt(format!(
                "system metadata item {} is marked as user metadata",
                guid_string(&guid)
            )));
        }
        let item = if length == 0 {
            if offset != 0 {
                return Err(DiskImageError::Corrupt(format!(
                    "empty metadata item {i} has non-zero offset {offset}"
                )));
            }
            &buf[0..0]
        } else {
            let end = offset
                .checked_add(length)
                .filter(|end| offset >= 64 * 1024 && length <= MB as usize && *end <= buf.len())
                .ok_or_else(|| {
                    DiskImageError::Corrupt(format!(
                        "metadata item {i} is {length} bytes at {offset}, outside the item area"
                    ))
                })?;
            if ranges
                .iter()
                .any(|(start, prior_end)| offset < *prior_end && *start < end)
            {
                return Err(DiskImageError::Corrupt(format!(
                    "metadata item {i} overlaps another item"
                )));
            }
            ranges.push((offset, end));
            &buf[offset..end]
        };
        match guid {
            FILE_PARAMETERS if item.len() == 8 => {
                md.block_size = Some(u32_at(item, 0));
                let flags = u32_at(item, 4);
                if flags & !0b11 != 0 {
                    return Err(DiskImageError::Corrupt(
                        "file-parameters metadata has unknown flags".into(),
                    ));
                }
                md.leave_blocks_allocated = flags & 1 != 0;
                md.has_parent = flags & 2 != 0;
            }
            VIRTUAL_DISK_SIZE if item.len() == 8 => md.virtual_size = Some(u64_at(item, 0)),
            LOGICAL_SECTOR_SIZE if item.len() == 4 => {
                md.logical_sector_size = Some(u32_at(item, 0))
            }
            PHYSICAL_SECTOR_SIZE if item.len() == 4 => {
                md.physical_sector_size = Some(u32_at(item, 0))
            }
            PARENT_LOCATOR => md.parent = Some(parse_parent_locator(item, limits)?),
            VIRTUAL_DISK_ID if item.len() == 16 => md.virtual_disk_id = Some(guid_at(item, 0)),
            FILE_PARAMETERS | VIRTUAL_DISK_SIZE | LOGICAL_SECTOR_SIZE | PHYSICAL_SECTOR_SIZE
            | VIRTUAL_DISK_ID => {
                return Err(DiskImageError::Corrupt(format!(
                    "metadata item {} has invalid length {}",
                    guid_string(&guid),
                    item.len()
                )))
            }
            _ if required => {
                return Err(DiskImageError::Unsupported(format!(
                    "metadata item {} is marked required but unknown",
                    guid_string(&guid)
                )))
            }
            _ => {}
        }
    }
    Ok(md)
}

fn parse_parent_locator(
    item: &[u8],
    limits: &DiskImageLimits,
) -> Result<ParentLocator, DiskImageError> {
    if item.len() < 20 {
        return Err(DiskImageError::Corrupt("parent locator header".into()));
    }
    let locator_guid = guid_at(item, 0);
    let locator_type = guid_string(&locator_guid);
    if locator_guid != VHDX_PARENT_LOCATOR_TYPE {
        return Err(DiskImageError::Unsupported(format!(
            "parent locator type {locator_type}"
        )));
    }
    if u16_at(item, 16) != 0 {
        return Err(DiskImageError::Corrupt(
            "parent locator reserved field is non-zero".into(),
        ));
    }
    let count = u16_at(item, 18) as usize;
    if count > limits.max_metadata_entries as usize {
        return Err(DiskImageError::Unsupported(format!(
            "parent locator needs {count} entries, above the configured limit of {}",
            limits.max_metadata_entries
        )));
    }
    let table_end = 20usize
        .checked_add(count.checked_mul(12).ok_or_else(|| {
            DiskImageError::Corrupt("parent locator entry table overflows".into())
        })?)
        .filter(|end| *end <= item.len())
        .ok_or_else(|| DiskImageError::Corrupt("parent locator entry table is truncated".into()))?;
    let mut entries = Vec::new();
    let mut keys = HashSet::with_capacity(count);
    let mut ranges = Vec::with_capacity(count.saturating_mul(2));
    for i in 0..count {
        let at = 20 + i * 12;
        if at + 12 > item.len() {
            return Err(DiskImageError::Corrupt(format!(
                "parent locator entry {i} runs past the item"
            )));
        }
        let key_offset = u32_at(item, at) as usize;
        let value_offset = u32_at(item, at + 4) as usize;
        let key_len = u16_at(item, at + 8) as usize;
        let value_len = u16_at(item, at + 10) as usize;
        if key_len % 2 != 0 || value_len % 2 != 0 {
            return Err(DiskImageError::Corrupt(
                "parent locator strings are not UTF-16-aligned".into(),
            ));
        }
        let slice = |offset: usize, len: usize| -> Result<(&[u8], usize), DiskImageError> {
            if len > limits.max_locator_bytes {
                return Err(DiskImageError::Unsupported(format!(
                    "parent locator string of {len} bytes exceeds the configured limit of {}",
                    limits.max_locator_bytes
                )));
            }
            match offset
                .checked_add(len)
                .filter(|end| offset >= table_end && *end <= item.len())
            {
                Some(end) => Ok((&item[offset..end], end)),
                None => Err(DiskImageError::Corrupt(
                    "parent locator string runs past the item".into(),
                )),
            }
        };
        let (key_bytes, key_end) = slice(key_offset, key_len)?;
        let (value_bytes, value_end) = slice(value_offset, value_len)?;
        for (start, end) in [(key_offset, key_end), (value_offset, value_end)] {
            if start != end
                && ranges
                    .iter()
                    .any(|&(other_start, other_end)| start < other_end && other_start < end)
            {
                return Err(DiskImageError::Corrupt(
                    "parent locator strings overlap".into(),
                ));
            }
            ranges.push((start, end));
        }
        let decode = |bytes: &[u8]| -> Result<String, DiskImageError> {
            let units = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>();
            let value = String::from_utf16(&units).map_err(|_| {
                DiskImageError::Corrupt("parent locator string is invalid UTF-16".into())
            })?;
            if value.contains('\0') {
                return Err(DiskImageError::Corrupt(
                    "parent locator string contains an embedded NUL".into(),
                ));
            }
            Ok(value)
        };
        let key = decode(key_bytes)?;
        if key.is_empty() {
            return Err(DiskImageError::Corrupt(
                "parent locator contains an empty key".into(),
            ));
        }
        if !keys.insert(key.clone()) {
            return Err(DiskImageError::Corrupt(format!(
                "parent locator contains duplicate key {key}"
            )));
        }
        entries.push((key, decode(value_bytes)?));
    }
    let parent_linkage = entries
        .iter()
        .find(|(key, _)| key == "parent_linkage")
        .map(|(_, value)| value);
    let valid_guid = |value: &str| {
        let bytes = value.as_bytes();
        bytes.len() == 38
            && bytes[0] == b'{'
            && bytes[37] == b'}'
            && [9, 14, 19, 24].into_iter().all(|at| bytes[at] == b'-')
            && bytes[1..37]
                .iter()
                .enumerate()
                .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
    };
    if !parent_linkage.is_some_and(|value| valid_guid(value)) {
        return Err(DiskImageError::Corrupt(
            "VHDX parent locator has no valid parent_linkage GUID".into(),
        ));
    }
    if keys.contains("parent_linkage2") {
        return Err(DiskImageError::Corrupt(
            "VHDX parent locator contains the forbidden parent_linkage2 entry".into(),
        ));
    }
    Ok(ParentLocator {
        locator_type,
        entries,
    })
}

impl<R: Read + Seek> VhdxDisk<R> {
    /// Parse the container of a VHDX file of `len` bytes and expose its
    /// virtual disk. Reads only the headers, region table, metadata, and BAT.
    pub fn open(mut inner: R, len: u64, limits: &DiskImageLimits) -> Result<Self, DiskImageError> {
        if len < MIN_FILE_LEN {
            return Err(DiskImageError::NotDiskImage);
        }
        let mut bytes_read = 0u64;
        let ident = read_at(&mut inner, 0, 520)?;
        bytes_read += 520;
        if &ident[..8] != FILE_SIGNATURE {
            return Err(DiskImageError::NotDiskImage);
        }
        let creator = utf16_string(&ident[8..520]);

        let mut header: Option<Header> = None;
        for offset in HEADER_OFFSETS {
            let buf = read_at(&mut inner, offset, HEADER_LEN)?;
            bytes_read += HEADER_LEN as u64;
            if let Some(h) = parse_header(&buf) {
                if header
                    .as_ref()
                    .is_none_or(|w| w.sequence_number < h.sequence_number)
                {
                    header = Some(h);
                }
            }
        }
        let header =
            header.ok_or_else(|| DiskImageError::Corrupt("neither header copy is valid".into()))?;
        if header.version != 1 {
            return Err(DiskImageError::Unsupported(format!(
                "VHDX header version {}",
                header.version
            )));
        }
        if header.log_version != 0 {
            return Err(DiskImageError::Unsupported(format!(
                "VHDX log version {}",
                header.log_version
            )));
        }
        if header.reserved_nonzero {
            return Err(DiskImageError::Corrupt(
                "VHDX header reserved bytes are non-zero".into(),
            ));
        }
        let log_end = header
            .log_offset
            .checked_add(header.log_length)
            .filter(|end| *end <= len)
            .ok_or_else(|| DiskImageError::Corrupt("VHDX log runs past the image".into()))?;
        if header.log_length == 0
            || header.log_length % MB != 0
            || header.log_offset < MB
            || header.log_offset % MB != 0
        {
            return Err(DiskImageError::Corrupt(format!(
                "VHDX log is {} bytes at offset {} rather than a non-empty MiB-aligned range",
                header.log_length, header.log_offset
            )));
        }

        // Both region tables are copies; take the first that parses.
        let mut regions = None;
        let mut region_error = None;
        for offset in REGION_TABLE_OFFSETS {
            let buf = read_at(&mut inner, offset, REGION_TABLE_LEN)?;
            bytes_read += REGION_TABLE_LEN as u64;
            match parse_region_table(&buf, len, limits) {
                Ok(r) => {
                    regions = Some(r);
                    break;
                }
                Err(e) => region_error = Some(e),
            }
        }
        let regions = match regions {
            Some(r) => r,
            None => return Err(region_error.unwrap_or(DiskImageError::NotDiskImage)),
        };
        for region in &regions.entries {
            if header.log_offset < region.offset + region.length && region.offset < log_end {
                return Err(DiskImageError::Corrupt(
                    "VHDX log overlaps a declared region".into(),
                ));
            }
        }
        let bat_region = regions
            .bat
            .ok_or_else(|| DiskImageError::Corrupt("no block allocation table region".into()))?;
        let metadata_region = regions.metadata.ok_or_else(|| {
            DiskImageError::Corrupt("no metadata region in the region table".into())
        })?;

        let span = metadata_region.length.min(METADATA_SPAN) as usize;
        let md_bytes = read_at(&mut inner, metadata_region.offset, span)?;
        bytes_read += span as u64;
        let md = parse_metadata(&md_bytes, limits)?;

        let block_size = md
            .block_size
            .ok_or_else(|| DiskImageError::Corrupt("no file-parameters metadata".into()))?;
        let virtual_size = md
            .virtual_size
            .ok_or_else(|| DiskImageError::Corrupt("no virtual-disk-size metadata".into()))?;
        let logical_sector_size = md
            .logical_sector_size
            .ok_or_else(|| DiskImageError::Corrupt("no logical-sector-size metadata".into()))?;
        let physical_sector_size = md
            .physical_sector_size
            .ok_or_else(|| DiskImageError::Corrupt("no physical-sector-size metadata".into()))?;
        let virtual_disk_id = md
            .virtual_disk_id
            .filter(|id| *id != [0u8; 16])
            .ok_or_else(|| DiskImageError::Corrupt("no valid virtual-disk ID metadata".into()))?;
        // MS-VHDX: block size is a power of two between 1 MiB and 256 MiB.
        if !(MB..=256 * MB).contains(&(block_size as u64)) || !block_size.is_power_of_two() {
            return Err(DiskImageError::Corrupt(format!(
                "block size {block_size} is not a power of two between 1 MiB and 256 MiB"
            )));
        }
        if logical_sector_size != 512 && logical_sector_size != 4096 {
            return Err(DiskImageError::Corrupt(format!(
                "logical sector size {logical_sector_size} is neither 512 nor 4096"
            )));
        }
        if physical_sector_size != 512 && physical_sector_size != 4096 {
            return Err(DiskImageError::Corrupt(format!(
                "physical sector size {physical_sector_size} is neither 512 nor 4096"
            )));
        }
        if virtual_size == 0
            || virtual_size % logical_sector_size as u64 != 0
            || virtual_size > SPEC_MAX_VIRTUAL_SIZE
        {
            return Err(DiskImageError::Corrupt(format!(
                "virtual disk size {virtual_size} is not a sector multiple at or below the VHDX maximum of {SPEC_MAX_VIRTUAL_SIZE} bytes"
            )));
        }
        if virtual_size > limits.max_virtual_size {
            return Err(DiskImageError::Unsupported(format!(
                "virtual disk size {virtual_size} exceeds the configured limit of {} bytes",
                limits.max_virtual_size
            )));
        }

        let payload = match (md.has_parent, md.leave_blocks_allocated) {
            (true, _) => PayloadKind::Differencing,
            (false, true) => PayloadKind::Fixed,
            (false, false) => PayloadKind::Dynamic,
        };
        let chunk_ratio = ((1u64 << 23) * logical_sector_size as u64) / block_size as u64;
        if chunk_ratio == 0 {
            return Err(DiskImageError::Corrupt(
                "block size exceeds one chunk of the block allocation table".into(),
            ));
        }
        let data_blocks = virtual_size.div_ceil(block_size as u64);
        // Fixed/dynamic tables end at the last payload entry; differencing
        // tables also include the sector bitmap entry for the final chunk.
        let bat_entries = data_blocks
            + if payload == PayloadKind::Differencing {
                data_blocks.div_ceil(chunk_ratio)
            } else {
                (data_blocks - 1) / chunk_ratio
            };

        let mut info = VhdxInfo {
            creator,
            virtual_disk_id: guid_string(&virtual_disk_id),
            payload,
            virtual_size,
            block_size,
            logical_sector_size,
            physical_sector_size,
            chunk_ratio,
            bat_entries,
            present_blocks: 0,
            sequence_number: header.sequence_number,
            log_replay_pending: header.log_guid != [0u8; 16],
            parent: md.parent,
        };
        let wanted = bat_entries.checked_mul(8).ok_or_else(|| {
            DiskImageError::Corrupt("block allocation table size overflows".into())
        })?;
        if wanted > bat_region.length {
            return Err(DiskImageError::Corrupt(format!(
                "block allocation table needs {wanted} bytes but its region is {}",
                bat_region.length
            )));
        }

        // A differencing disk's BAT describes divergence from a parent that
        // this crate does not resolve, so it is never loaded.
        let bat = if payload == PayloadKind::Differencing {
            if info.parent.is_none() {
                return Err(DiskImageError::Corrupt(
                    "the has-parent flag is set but no parent locator is present".into(),
                ));
            }
            Vec::new()
        } else {
            if info.parent.is_some() {
                return Err(DiskImageError::Corrupt(
                    "a parent locator is present but the has-parent flag is not set".into(),
                ));
            }
            let region = bat_region;
            if bat_entries > limits.max_bat_entries {
                return Err(DiskImageError::Unsupported(format!(
                    "block allocation table needs {bat_entries} entries, above the configured limit of {}",
                    limits.max_bat_entries
                )));
            }
            let wanted_usize = usize::try_from(wanted).map_err(|_| {
                DiskImageError::Corrupt(
                    "block allocation table does not fit this platform's address space".into(),
                )
            })?;
            let raw = read_at(&mut inner, region.offset, wanted_usize)?;
            bytes_read += wanted;
            // Drop the sector-bitmap entry that follows every chunk_ratio
            // payload entries, leaving a plain block-indexed array.
            let capacity = usize::try_from(data_blocks).map_err(|_| {
                DiskImageError::Corrupt(
                    "payload block count does not fit this platform's address space".into(),
                )
            })?;
            let mut bat = Vec::new();
            bat.try_reserve_exact(capacity).map_err(|_| {
                DiskImageError::Unsupported(
                    "payload block allocation exceeds available memory".into(),
                )
            })?;
            let mut referenced_blocks = 0usize;
            for block in 0..data_blocks {
                let index = block + block / chunk_ratio;
                if block != 0 && block % chunk_ratio == 0 {
                    let bitmap_index = index - 1;
                    let bitmap = u64_at(&raw, bitmap_index as usize * 8);
                    if bitmap & 7 != 0 || bitmap & 0x000f_fff8 != 0 {
                        return Err(DiskImageError::Corrupt(format!(
                            "sector bitmap BAT entry {bitmap_index} is not an unallocated state-0 entry"
                        )));
                    }
                }
                let entry = u64_at(&raw, index as usize * 8);
                let state = entry & 7;
                if entry & 0x000f_fff8 != 0 {
                    return Err(DiskImageError::Corrupt(format!(
                        "payload BAT entry {block} has non-zero reserved bits"
                    )));
                }
                if state == PAYLOAD_BLOCK_PARTIALLY_PRESENT {
                    return Err(DiskImageError::Corrupt(format!(
                        "payload BAT entry {block} is partially present without a parent"
                    )));
                }
                if matches!(state, 4 | 5) {
                    return Err(DiskImageError::Corrupt(format!(
                        "payload BAT entry {block} uses reserved state {state}"
                    )));
                }
                let file_offset = (entry >> 20) * MB;
                let references_payload = state == PAYLOAD_BLOCK_FULLY_PRESENT
                    || (matches!(state, 1 | 3) && file_offset != 0);
                if references_payload {
                    referenced_blocks += 1;
                    let file_end = file_offset
                        .checked_add(u64::from(block_size))
                        .filter(|end| *end <= len)
                        .ok_or_else(|| {
                            DiskImageError::Corrupt(format!(
                                "payload BAT entry {block} at {file_offset} runs past the {len} byte image"
                            ))
                        })?;
                    let overlaps = |start: u64, length: u64| {
                        let end = start + length;
                        file_offset < end && start < file_end
                    };
                    if file_offset < MB
                        || overlaps(header.log_offset, header.log_length)
                        || regions
                            .entries
                            .iter()
                            .any(|declared| overlaps(declared.offset, declared.length))
                    {
                        return Err(DiskImageError::Corrupt(format!(
                            "payload BAT entry {block} overlaps VHDX container metadata"
                        )));
                    }
                }
                bat.push(entry);
            }
            // The raw BAT and retained payload BAT together already use 16
            // bytes per payload entry while opening. Drop the raw copy before
            // allocating a compact 8-byte start-offset list for the global
            // overlap check; a tree node per block would multiply the memory
            // promised by max_bat_entries.
            drop(raw);
            let mut physical_starts = Vec::new();
            physical_starts
                .try_reserve_exact(referenced_blocks)
                .map_err(|_| {
                    DiskImageError::Unsupported(
                        "payload overlap validation exceeds available memory".into(),
                    )
                })?;
            physical_starts.extend(bat.iter().filter_map(|entry| {
                let state = entry & 7;
                let file_offset = (entry >> 20) * MB;
                (state == PAYLOAD_BLOCK_FULLY_PRESENT
                    || (matches!(state, 1 | 3) && file_offset != 0))
                    .then_some(file_offset)
            }));
            physical_starts.sort_unstable();
            if let Some(pair) = physical_starts
                .windows(2)
                .find(|pair| pair[1] < pair[0] + u64::from(block_size))
            {
                return Err(DiskImageError::Corrupt(format!(
                    "payload block at offset {} overlaps another referenced payload block at {}",
                    pair[1], pair[0]
                )));
            }
            info.present_blocks = bat
                .iter()
                .filter(|e| *e & 7 == PAYLOAD_BLOCK_FULLY_PRESENT)
                .count() as u64;
            bat
        };

        Ok(Self {
            inner,
            info,
            bat,
            file_len: len,
            pos: 0,
            bytes_read,
        })
    }

    pub fn info(&self) -> &VhdxInfo {
        &self.info
    }

    /// Bytes read from the image file so far, container metadata included.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    pub fn virtual_size(&self) -> u64 {
        self.info.virtual_size
    }
}

impl<R: Read + Seek> Read for VhdxDisk<R> {
    /// Reads never cross a payload block boundary; callers loop, which is
    /// what `Read::read_exact` and `BufReader` already do.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.info.virtual_size.saturating_sub(self.pos);
        if remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        if self.info.log_replay_pending {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VHDX payload cannot be read until its pending log is replayed",
            ));
        }
        if self.info.payload == PayloadKind::Differencing {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "differencing VHDX payload requires its parent image",
            ));
        }
        let block_size = self.info.block_size as u64;
        let block = self.pos / block_size;
        let within = self.pos % block_size;
        let want = buf.len().min(
            (block_size - within)
                .min(remaining)
                .try_into()
                .unwrap_or(usize::MAX),
        );
        let entry = *self.bat.get(block as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("block {block} is outside the block allocation table"),
            )
        })?;
        let state = entry & 7;
        if state == PAYLOAD_BLOCK_FULLY_PRESENT {
            // Bits 20..64 hold the file offset in megabytes.
            let file_offset = (entry >> 20) * MB;
            let end = file_offset
                .checked_add(within)
                .and_then(|s| s.checked_add(want as u64))
                .filter(|end| *end <= self.file_len)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "block {block} at {file_offset} runs past the {} byte image",
                            self.file_len
                        ),
                    )
                })?;
            self.inner.seek(SeekFrom::Start(end - want as u64))?;
            self.inner.read_exact(&mut buf[..want])?;
            self.bytes_read += want as u64;
        } else if matches!(state, 0..=3) {
            // Not present, explicitly zero, or unmapped in a non-differencing
            // image may all use the specification's permitted zero-return
            // behaviour. State 1 is explicitly undefined, for which zero is
            // likewise one allowed result.
            buf[..want].fill(0);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("block {block} has undefined BAT state {state}"),
            ));
        }
        self.pos += want as u64;
        Ok(want)
    }
}

impl<R: Read + Seek> Seek for VhdxDisk<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(n) => self.info.virtual_size.checked_add_signed(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
        };
        self.pos = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek outside the virtual disk")
        })?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{build_vhdx, VhdxSpec};
    use std::io::Cursor;

    const TEST_BAT_OFFSET: usize = (2 * MB) as usize;

    fn open(bytes: &[u8]) -> Result<VhdxDisk<Cursor<Vec<u8>>>, DiskImageError> {
        let len = bytes.len() as u64;
        VhdxDisk::open(
            Cursor::new(bytes.to_vec()),
            len,
            &DiskImageLimits::default(),
        )
    }

    fn rewrite_header_crc(image: &mut [u8], at: usize) {
        image[at + 4..at + 8].fill(0);
        let sum = crc32c(&image[at..at + HEADER_LEN]);
        image[at + 4..at + 8].copy_from_slice(&sum.to_le_bytes());
    }

    fn rewrite_region_crc(image: &mut [u8], at: usize) {
        image[at + 4..at + 8].fill(0);
        let sum = crc32c(&image[at..at + REGION_TABLE_LEN]);
        image[at + 4..at + 8].copy_from_slice(&sum.to_le_bytes());
    }

    fn parent_locator(pairs: &[(&str, &str)]) -> Vec<u8> {
        let encode = |value: &str| {
            value
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        };
        let mut item = Vec::from(VHDX_PARENT_LOCATOR_TYPE);
        item.extend_from_slice(&0u16.to_le_bytes());
        item.extend_from_slice(&(pairs.len() as u16).to_le_bytes());
        let data_offset = 20 + pairs.len() * 12;
        let mut data = Vec::new();
        for (key, value) in pairs {
            let key = encode(key);
            let value = encode(value);
            let key_offset = data_offset + data.len();
            data.extend_from_slice(&key);
            let value_offset = data_offset + data.len();
            data.extend_from_slice(&value);
            item.extend_from_slice(&(key_offset as u32).to_le_bytes());
            item.extend_from_slice(&(value_offset as u32).to_le_bytes());
            item.extend_from_slice(&(key.len() as u16).to_le_bytes());
            item.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        item.extend_from_slice(&data);
        item
    }

    #[test]
    fn crc32c_is_castagnoli_not_ieee() {
        // The check value every CRC-32C implementation agrees on.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        // IEEE CRC-32 has a different check value for the same bytes.
        assert_ne!(crc32c(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn guid_rendering_is_windows_serialised() {
        assert_eq!(
            guid_string(&BAT_REGION),
            "2dc27766-f623-4200-9d64-115e9bfd4a08"
        );
    }

    #[test]
    fn dynamic_payload_reads_through_the_bat() {
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let image = build_vhdx(&VhdxSpec {
            payload: payload.clone(),
            ..VhdxSpec::default()
        });
        let mut disk = open(&image).unwrap();
        assert_eq!(disk.info().payload, PayloadKind::Dynamic);
        assert_eq!(disk.info().virtual_size, 2 * MB);
        assert_eq!(
            disk.info().virtual_disk_id,
            "01010101-0101-0101-0101-010101010101"
        );
        assert_eq!(disk.info().block_size, MB as u32);
        assert_eq!(disk.info().chunk_ratio, 4096);
        // One block holds the payload; the second is unallocated.
        assert_eq!(disk.info().bat_entries, 2);
        assert_eq!(disk.info().present_blocks, 1);

        let mut got = vec![0u8; payload.len()];
        disk.read_exact(&mut got).unwrap();
        assert_eq!(got, payload);

        // A read that spans the end of the stored block still succeeds and
        // the unallocated block reads as zero.
        disk.seek(SeekFrom::Start(MB - 8)).unwrap();
        let mut across = [1u8; 16];
        disk.read_exact(&mut across).unwrap();
        assert_eq!(across, [0u8; 16]);
        assert!(disk.bytes_read() > 0);

        // Reading past the end of the virtual disk stops cleanly.
        disk.seek(SeekFrom::Start(2 * MB)).unwrap();
        assert_eq!(disk.read(&mut [0u8; 8]).unwrap(), 0);
    }

    #[test]
    fn fixed_payloads_allocate_every_block() {
        let image = build_vhdx(&VhdxSpec {
            fixed: true,
            payload: vec![0xAB; 8],
            ..VhdxSpec::default()
        });
        let disk = open(&image).unwrap();
        assert_eq!(disk.info().payload, PayloadKind::Fixed);
        assert_eq!(disk.info().present_blocks, 2);
    }

    #[test]
    fn differencing_images_expose_their_parent_locator() {
        let image = build_vhdx(&VhdxSpec {
            differencing: true,
            parent_path: Some(".\\basin.vhdx".into()),
            ..VhdxSpec::default()
        });
        let mut disk = open(&image).unwrap();
        assert_eq!(disk.info().payload, PayloadKind::Differencing);
        let parent = disk.info().parent.as_ref().unwrap();
        assert_eq!(parent.locator_type, "b04aefb7-d19e-4a81-b789-25b8e9445913");
        assert_eq!(parent.get("relative_path"), Some(".\\basin.vhdx"));
        assert_eq!(parent.get("volume_path"), None);
        assert_eq!(disk.info().bat_entries, 3);
        // The BAT of a differencing image is never interpreted.
        assert_eq!(disk.info().present_blocks, 0);
        let error = disk
            .read(&mut [0u8; 1])
            .expect_err("a parentless differencing payload must not be read");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("parent image"), "{error}");
    }

    #[test]
    fn differencing_bat_region_must_cover_the_declared_table() {
        let mut image = build_vhdx(&VhdxSpec {
            differencing: true,
            parent_path: Some(".\\basin.vhdx".into()),
            ..VhdxSpec::default()
        });
        // Enlarge only the declared virtual disk. A 256-GiB differencing
        // image needs more than the fixture's 1-MiB BAT region.
        let metadata = (3 * MB) as usize;
        let virtual_size_entry = metadata + METADATA_TABLE_HEADER_LEN + METADATA_ENTRY_LEN;
        let item = metadata + u32_at(&image, virtual_size_entry + 16) as usize;
        image[item..item + 8].copy_from_slice(&(256 * 1024u64.pow(3)).to_le_bytes());

        match open(&image) {
            Err(DiskImageError::Corrupt(message)) => {
                assert!(
                    message.contains("BAT region") || message.contains("its region"),
                    "{message}"
                )
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn malformed_parent_locators_are_rejected() {
        let limits = DiskImageLimits::default();
        let linkage = "{00000000-0000-0000-0000-000000000001}";

        let mut unknown = parent_locator(&[("parent_linkage", linkage)]);
        unknown[0] ^= 0xFF;
        assert!(matches!(
            parse_parent_locator(&unknown, &limits),
            Err(DiskImageError::Unsupported(_))
        ));

        let missing = parent_locator(&[("relative_path", ".\\basin.vhdx")]);
        match parse_parent_locator(&missing, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("parent_linkage"), "{m}"),
            other => panic!("{other:?}"),
        }

        let malformed_linkage = parent_locator(&[("parent_linkage", "not-a-guid")]);
        match parse_parent_locator(&malformed_linkage, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("valid parent_linkage"), "{m}"),
            other => panic!("{other:?}"),
        }

        let duplicate = parent_locator(&[("parent_linkage", linkage), ("parent_linkage", linkage)]);
        match parse_parent_locator(&duplicate, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("duplicate key"), "{m}"),
            other => panic!("{other:?}"),
        }

        let mut invalid_utf16 = parent_locator(&[("parent_linkage", linkage)]);
        let value_offset = u32_at(&invalid_utf16, 24) as usize;
        invalid_utf16[value_offset..value_offset + 2].copy_from_slice(&0xD800u16.to_le_bytes());
        match parse_parent_locator(&invalid_utf16, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("invalid UTF-16"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_differencing_image_without_a_parent_locator_is_corrupt() {
        let image = build_vhdx(&VhdxSpec {
            differencing: true,
            ..VhdxSpec::default()
        });
        match open(&image) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("no parent locator"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn a_parent_locator_without_the_has_parent_flag_is_corrupt() {
        let image = build_vhdx(&VhdxSpec {
            parent_path: Some(".\\basin.vhdx".into()),
            ..VhdxSpec::default()
        });
        match open(&image) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("has-parent flag"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn log_guid_flags_a_pending_replay() {
        let image = build_vhdx(&VhdxSpec {
            log_guid: [7u8; 16],
            ..VhdxSpec::default()
        });
        let mut disk = open(&image).unwrap();
        assert!(disk.info().log_replay_pending);
        let error = disk.read(&mut [0u8; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("pending log"));
    }

    #[test]
    fn one_good_copy_of_each_structure_is_enough() {
        let good = build_vhdx(&VhdxSpec::default());
        // Header 2 has the higher sequence number; damaging it falls back.
        let mut damaged = good.clone();
        damaged[0x2_0000 + 100] ^= 0xFF;
        assert_eq!(open(&damaged).unwrap().info().sequence_number, 1);
        // Damaging region table 1 falls back to its copy.
        let mut damaged = good.clone();
        damaged[0x3_0000 + 20] ^= 0xFF;
        assert!(open(&damaged).is_ok());
    }

    #[test]
    fn the_current_header_controls_version_and_log_geometry() {
        let mut unsupported = build_vhdx(&VhdxSpec::default());
        let current = 0x2_0000;
        unsupported[current + 66..current + 68].copy_from_slice(&2u16.to_le_bytes());
        rewrite_header_crc(&mut unsupported, current);
        match open(&unsupported) {
            Err(DiskImageError::Unsupported(m)) => assert!(m.contains("version 2"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }

        let mut overlapping = build_vhdx(&VhdxSpec::default());
        overlapping[current + 72..current + 80].copy_from_slice(&(2 * MB).to_le_bytes());
        rewrite_header_crc(&mut overlapping, current);
        match open(&overlapping) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("log overlaps"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }

        let mut bad_log_version = build_vhdx(&VhdxSpec::default());
        bad_log_version[current + 64..current + 66].copy_from_slice(&1u16.to_le_bytes());
        rewrite_header_crc(&mut bad_log_version, current);
        match open(&bad_log_version) {
            Err(DiskImageError::Unsupported(m)) => assert!(m.contains("log version 1"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }

        let mut reserved = build_vhdx(&VhdxSpec::default());
        reserved[current + 80] = 1;
        rewrite_header_crc(&mut reserved, current);
        match open(&reserved) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("reserved bytes"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn required_container_regions_cannot_be_marked_optional() {
        let mut image = build_vhdx(&VhdxSpec::default());
        for table in [0x3_0000usize, 0x4_0000] {
            image[table + 16 + 28..table + 16 + 32].fill(0);
            rewrite_region_crc(&mut image, table);
        }
        match open(&image) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("marked required"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn metadata_items_must_not_overlap() {
        let mut image = build_vhdx(&VhdxSpec::default());
        let metadata = (3 * MB) as usize;
        let first_item_offset = u32_at(&image, metadata + 32 + 16);
        image[metadata + 64 + 16..metadata + 64 + 20]
            .copy_from_slice(&first_item_offset.to_le_bytes());
        match open(&image) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("overlaps"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn the_required_virtual_disk_id_cannot_be_nil() {
        let mut image = build_vhdx(&VhdxSpec::default());
        let metadata = (3 * MB) as usize;
        let id_entry = metadata + METADATA_TABLE_HEADER_LEN + 4 * METADATA_ENTRY_LEN;
        let id_offset = u32_at(&image, id_entry + 16) as usize;
        image[metadata + id_offset..metadata + id_offset + 16].fill(0);
        match open(&image) {
            Err(DiskImageError::Corrupt(message)) => {
                assert!(message.contains("virtual-disk ID"), "{message}")
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn corrupt_and_truncated_images_fail_cleanly() {
        let good = build_vhdx(&VhdxSpec::default());
        assert!(matches!(
            open(&good[..1000]),
            Err(DiskImageError::NotDiskImage)
        ));
        let junk = vec![0x5Au8; 2 * MB as usize];
        assert!(matches!(open(&junk), Err(DiskImageError::NotDiskImage)));
        // Both header copies damaged.
        let mut headless = good.clone();
        headless[0x1_0000 + 100] ^= 0xFF;
        headless[0x2_0000 + 100] ^= 0xFF;
        match open(&headless) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("neither header"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
        // Both region tables damaged.
        let mut regionless = good.clone();
        regionless[0x3_0000 + 20] ^= 0xFF;
        regionless[0x4_0000 + 20] ^= 0xFF;
        match open(&regionless) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("checksum"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
        // Truncated midway through the declared log.
        match open(&good[..(1500 * 1024)]) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("log runs past"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn implausible_geometry_is_rejected_before_it_allocates() {
        let cases: [(VhdxSpec, &str); 3] = [
            (
                VhdxSpec {
                    block_size: 512,
                    ..VhdxSpec::default()
                },
                "block size",
            ),
            (
                VhdxSpec {
                    logical_sector_size: 1024,
                    ..VhdxSpec::default()
                },
                "logical sector size",
            ),
            (
                VhdxSpec {
                    virtual_size: 2 * MB + 1,
                    ..VhdxSpec::default()
                },
                "virtual disk size",
            ),
        ];
        for (spec, expected) in cases {
            let image = build_vhdx(&spec);
            match open(&image) {
                Err(DiskImageError::Corrupt(m)) => assert!(m.contains(expected), "{m}"),
                other => panic!("{expected}: {:?}", other.map(|_| ())),
            }
        }
    }

    #[test]
    fn budgets_cap_declared_sizes_and_counts() {
        let image = build_vhdx(&VhdxSpec::default());
        let len = image.len() as u64;
        let small = DiskImageLimits {
            max_virtual_size: MB,
            ..DiskImageLimits::default()
        };
        match VhdxDisk::open(Cursor::new(image.clone()), len, &small) {
            Err(DiskImageError::Unsupported(m)) => {
                assert!(m.contains("configured limit"), "{m}")
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        let small = DiskImageLimits {
            max_bat_entries: 1,
            ..DiskImageLimits::default()
        };
        match VhdxDisk::open(Cursor::new(image.clone()), len, &small) {
            Err(DiskImageError::Unsupported(m)) => {
                assert!(m.contains("block allocation table needs 2 entries"), "{m}")
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        let small = DiskImageLimits {
            max_metadata_entries: 1,
            ..DiskImageLimits::default()
        };
        match VhdxDisk::open(Cursor::new(image), len, &small) {
            Err(DiskImageError::Unsupported(m)) => {
                assert!(m.contains("metadata table needs"), "{m}")
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn a_bat_entry_pointing_outside_the_file_is_rejected_when_opened() {
        let mut image = build_vhdx(&VhdxSpec {
            payload: vec![9u8; 64],
            ..VhdxSpec::default()
        });
        // Repoint block 0 at megabyte 4096, far past the end of the image.
        let entry: u64 = (4096u64 << 20) | 6;
        image[TEST_BAT_OFFSET..TEST_BAT_OFFSET + 8].copy_from_slice(&entry.to_le_bytes());
        match open(&image) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("runs past"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn payload_block_ranges_cannot_overlap() {
        let mut image = build_vhdx(&VhdxSpec {
            block_size: (2 * MB) as u32,
            virtual_size: 4 * MB,
            payload: vec![9u8; (4 * MB) as usize],
            ..VhdxSpec::default()
        });
        // The fixture places the two 2-MiB blocks at 4 and 6 MiB. Moving the
        // second to 5 MiB gives it a distinct but overlapping BAT offset.
        let second = (5 << 20) | PAYLOAD_BLOCK_FULLY_PRESENT;
        image[TEST_BAT_OFFSET + 8..TEST_BAT_OFFSET + 16].copy_from_slice(&second.to_le_bytes());
        match open(&image) {
            Err(DiskImageError::Corrupt(m)) => {
                assert!(m.contains("overlaps another referenced payload"), "{m}")
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn payload_and_sector_bitmap_states_follow_the_vhdx_rules() {
        let dynamic = build_vhdx(&VhdxSpec {
            payload: vec![9u8; 64],
            ..VhdxSpec::default()
        });
        let mut partial = dynamic.clone();
        let entry = u64::from_le_bytes(
            partial[TEST_BAT_OFFSET..TEST_BAT_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
        partial[TEST_BAT_OFFSET..TEST_BAT_OFFSET + 8]
            .copy_from_slice(&((entry & !7) | PAYLOAD_BLOCK_PARTIALLY_PRESENT).to_le_bytes());
        match open(&partial) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("partially present"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }

        let mut fixed = build_vhdx(&VhdxSpec {
            fixed: true,
            ..VhdxSpec::default()
        });
        fixed[TEST_BAT_OFFSET..TEST_BAT_OFFSET + 8].fill(0);
        let mut fixed = open(&fixed).unwrap();
        let mut zero = [1u8; 8];
        fixed.read_exact(&mut zero).unwrap();
        assert_eq!(zero, [0; 8]);

        let mut undefined = dynamic.clone();
        undefined[TEST_BAT_OFFSET..TEST_BAT_OFFSET + 8].copy_from_slice(&1u64.to_le_bytes());
        let mut undefined = open(&undefined).unwrap();
        undefined.read_exact(&mut zero).unwrap();
        assert_eq!(zero, [0; 8]);

        // At one block beyond a full chunk, the preceding BAT slot is the
        // sector-bitmap entry. Fixed/dynamic images require state 0 there.
        let mut bad_bitmap = build_vhdx(&VhdxSpec {
            virtual_size: (4096 + 1) * MB,
            ..VhdxSpec::default()
        });
        let bitmap_at = TEST_BAT_OFFSET + 4096 * 8;
        bad_bitmap[bitmap_at..bitmap_at + 8]
            .copy_from_slice(&PAYLOAD_BLOCK_FULLY_PRESENT.to_le_bytes());
        match open(&bad_bitmap) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("sector bitmap"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }

        let mut metadata_overlap = dynamic;
        let entry = (3 << 20) | PAYLOAD_BLOCK_FULLY_PRESENT;
        metadata_overlap[TEST_BAT_OFFSET..TEST_BAT_OFFSET + 8]
            .copy_from_slice(&entry.to_le_bytes());
        match open(&metadata_overlap) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("overlaps"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }
}
