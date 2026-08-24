//! Partition enumeration over a virtual disk: GPT first, MBR as a fallback,
//! with each partition's first sector probed for the NTFS boot signature.
//!
//! As with the container, nothing declared by the table is trusted. Entry
//! counts and sizes are capped before allocation, and every partition must
//! fit the virtual disk, so a crafted table cannot make a later filesystem
//! reader address bytes outside the image.

use crate::{DiskImageError, DiskImageLimits};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_HEADER_LEN: usize = 92;
const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];
const MBR_TABLE_OFFSET: usize = 0x1BE;
const MBR_ENTRY_LEN: usize = 16;
/// NTFS puts its OEM identifier eight bytes into the boot sector.
const NTFS_OEM: &[u8; 8] = b"NTFS    ";
/// GPT partition entries are 128 bytes; larger ones are permitted but are a
/// sign of a crafted table rather than a real disk.
const MAX_GPT_ENTRY_LEN: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionScheme {
    Gpt,
    Mbr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    /// Position in the table, counting entries that were skipped.
    pub index: u32,
    /// First byte of the partition within the virtual disk.
    pub start: u64,
    pub length: u64,
    /// GPT type GUID, or `mbr-XX` for the MBR type byte.
    pub type_label: String,
    /// GPT partition name; empty under MBR.
    pub name: String,
    /// The first sector carries the NTFS boot signature.
    pub ntfs: bool,
    /// An MBR extended partition; its logical partitions are not walked.
    pub extended: bool,
}

pub struct Table {
    pub scheme: Option<PartitionScheme>,
    pub partitions: Vec<Partition>,
    /// The result cannot claim to enumerate every partition.
    pub truncated: bool,
    /// Why the table is only a partial view, including unsupported linked
    /// structures such as extended MBR partitions.
    pub incomplete_reason: Option<String>,
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

/// Read the partition table of a virtual disk of `disk_len` bytes.
///
/// A disk with neither a GPT header nor an MBR signature is not an error:
/// the caller simply finds no NTFS volumes.
pub fn read_table<R: Read + Seek>(
    r: &mut R,
    disk_len: u64,
    sector_size: u32,
    limits: &DiskImageLimits,
) -> Result<Table, DiskImageError> {
    if !matches!(sector_size, 512 | 4096) {
        return Err(DiskImageError::Unsupported(format!(
            "partition discovery requires a 512- or 4096-byte logical sector, not {sector_size}"
        )));
    }
    let sector = u64::from(sector_size);
    let mut table = match read_gpt(r, disk_len, sector, limits)? {
        Some(t) => t,
        None => read_mbr(r, disk_len, sector, limits)?,
    };
    for partition in &mut table.partitions {
        partition.ntfs = !partition.extended && probe_ntfs(r, partition.start, partition.length)?;
    }
    Ok(table)
}

/// True when the sector at `start` carries an NTFS boot record.
fn probe_ntfs<R: Read + Seek>(r: &mut R, start: u64, length: u64) -> Result<bool, DiskImageError> {
    if length < 512 {
        return Ok(false);
    }
    let mut boot = [0u8; 512];
    r.seek(SeekFrom::Start(start))?;
    r.read_exact(&mut boot)?;
    Ok(&boot[3..11] == NTFS_OEM && boot[510..512] == MBR_SIGNATURE)
}

fn read_gpt<R: Read + Seek>(
    r: &mut R,
    disk_len: u64,
    sector: u64,
    limits: &DiskImageLimits,
) -> Result<Option<Table>, DiskImageError> {
    let sector_count = disk_len / sector;
    if sector_count < 2 {
        return Ok(None);
    }
    let primary_error = match read_gpt_at(r, disk_len, sector, 1, limits) {
        Ok(Some(table)) => return Ok(Some(table)),
        Ok(None) => None,
        Err(error) => Some(error),
    };
    match read_gpt_at(r, disk_len, sector, sector_count - 1, limits) {
        Ok(Some(table)) => Ok(Some(table)),
        Ok(None) => primary_error.map_or(Ok(None), Err),
        Err(backup_error) => Err(primary_error.unwrap_or(backup_error)),
    }
}

fn read_gpt_at<R: Read + Seek>(
    r: &mut R,
    disk_len: u64,
    sector: u64,
    header_lba: u64,
    limits: &DiskImageLimits,
) -> Result<Option<Table>, DiskImageError> {
    let mut header = vec![0u8; sector as usize];
    r.seek(SeekFrom::Start(header_lba * sector))?;
    r.read_exact(&mut header)?;
    if &header[..8] != GPT_SIGNATURE {
        return Ok(None);
    }
    let header_size = u32_at(&header, 12) as usize;
    if !(GPT_HEADER_LEN..=header.len()).contains(&header_size) {
        return Err(DiskImageError::Corrupt(format!(
            "GPT header claims {header_size} bytes, outside one {sector} byte sector"
        )));
    }
    let stored = u32_at(&header, 16);
    let mut zeroed = header[..header_size].to_vec();
    zeroed[16..20].fill(0);
    if crc32fast::hash(&zeroed) != stored {
        return Err(DiskImageError::Corrupt("GPT header checksum".into()));
    }
    if u32_at(&header, 8) != 0x0001_0000 || u32_at(&header, 20) != 0 {
        return Err(DiskImageError::Corrupt(
            "GPT header has an unsupported revision or non-zero reserved field".into(),
        ));
    }

    let current_lba = u64_at(&header, 24);
    let alternate_lba = u64_at(&header, 32);
    let first_usable = u64_at(&header, 40);
    let last_usable = u64_at(&header, 48);
    let sector_count = disk_len / sector;
    if header[56..72].iter().all(|byte| *byte == 0) {
        return Err(DiskImageError::Corrupt(
            "GPT header has an all-zero disk GUID".into(),
        ));
    }
    let expected_alternate = if header_lba == 1 { sector_count - 1 } else { 1 };
    if current_lba != header_lba
        || alternate_lba != expected_alternate
        || first_usable > last_usable
        || first_usable <= 1
        || last_usable >= sector_count - 1
    {
        return Err(DiskImageError::Corrupt(format!(
            "GPT header at LBA {header_lba} has inconsistent disk geometry"
        )));
    }

    let entry_lba = u64_at(&header, 72);
    let entry_count = u32_at(&header, 80);
    let entry_len = u32_at(&header, 84);
    let entry_crc = u32_at(&header, 88);
    if !(128..=MAX_GPT_ENTRY_LEN).contains(&entry_len) || entry_len % 8 != 0 {
        return Err(DiskImageError::Corrupt(format!(
            "GPT partition entries are {entry_len} bytes"
        )));
    }
    if entry_count == 0 {
        return Err(DiskImageError::Corrupt(
            "GPT partition array declares no entries".into(),
        ));
    }
    let array_start = entry_lba
        .checked_mul(sector)
        .filter(|s| *s < disk_len)
        .ok_or_else(|| {
            DiskImageError::Corrupt(format!("GPT entry array starts at LBA {entry_lba}"))
        })?;
    let array_len = u64::from(entry_count)
        .checked_mul(u64::from(entry_len))
        .ok_or_else(|| DiskImageError::Corrupt("GPT partition array size overflows".into()))?;
    let array_end = array_start
        .checked_add(array_len)
        .filter(|end| *end <= disk_len)
        .ok_or_else(|| {
            DiskImageError::Corrupt(format!(
                "GPT partition array at {array_start} is {array_len} bytes, outside a {disk_len} byte disk"
            ))
        })?;
    let array_lbas = array_len.div_ceil(sector);
    let array_end_lba = entry_lba
        .checked_add(array_lbas)
        .ok_or_else(|| DiskImageError::Corrupt("GPT partition array LBA range overflows".into()))?;
    let usable_end = last_usable + 1;
    if entry_lba < usable_end && first_usable < array_end_lba {
        return Err(DiskImageError::Corrupt(
            "GPT partition array overlaps the usable disk range".into(),
        ));
    }
    if [current_lba, alternate_lba]
        .into_iter()
        .any(|lba| entry_lba <= lba && lba < array_end_lba)
    {
        return Err(DiskImageError::Corrupt(
            "GPT partition array overlaps a GPT header".into(),
        ));
    }
    if array_len > limits.max_partition_table_bytes {
        return Ok(Some(Table {
            scheme: Some(PartitionScheme::Gpt),
            partitions: Vec::new(),
            truncated: true,
            incomplete_reason: Some(format!(
                "GPT partition array is {array_len} bytes, exceeding the {} byte checksum budget",
                limits.max_partition_table_bytes
            )),
        }));
    }

    // The entry-array checksum covers every declared entry, not just the
    // subset retained under max_partitions. Trust no entries unless the
    // complete, independently bounded array validates.
    let mut hasher = crc32fast::Hasher::new();
    let mut at = array_start;
    let mut remaining = array_len;
    let mut chunk = vec![0u8; 64 * 1024];
    while remaining != 0 {
        let wanted = remaining.min(chunk.len() as u64) as usize;
        r.seek(SeekFrom::Start(at))?;
        r.read_exact(&mut chunk[..wanted])?;
        hasher.update(&chunk[..wanted]);
        at += wanted as u64;
        remaining -= wanted as u64;
    }
    if hasher.finalize() != entry_crc {
        return Err(DiskImageError::Corrupt(
            "GPT partition array checksum".into(),
        ));
    }

    let truncated = entry_count as u64 > limits.max_partitions as u64;
    let wanted = (u64::from(entry_count)).min(limits.max_partitions as u64) as u32;

    let mut partitions: Vec<Partition> = Vec::new();
    let mut partition_guids = HashSet::new();
    let mut entry = vec![0u8; entry_len as usize];
    for index in 0..wanted {
        let at = array_start + u64::from(index) * u64::from(entry_len);
        debug_assert!(at + u64::from(entry_len) <= array_end);
        r.seek(SeekFrom::Start(at))?;
        r.read_exact(&mut entry)?;
        let type_guid = {
            let mut g = [0u8; 16];
            g.copy_from_slice(&entry[..16]);
            g
        };
        if type_guid == [0u8; 16] {
            continue;
        }
        let partition_guid = {
            let mut guid = [0u8; 16];
            guid.copy_from_slice(&entry[16..32]);
            guid
        };
        if partition_guid == [0u8; 16] || !partition_guids.insert(partition_guid) {
            return Err(DiskImageError::Corrupt(format!(
                "GPT entry {index} has a missing or duplicate unique partition GUID"
            )));
        }
        let first = u64_at(&entry, 32);
        let last = u64_at(&entry, 40);
        if last < first {
            return Err(DiskImageError::Corrupt(format!(
                "GPT entry {index} ends at LBA {last} before it starts at {first}"
            )));
        }
        let start = first.checked_mul(sector);
        // EndingLBA is inclusive.
        let end = last.checked_add(1).and_then(|l| l.checked_mul(sector));
        let (start, end) = match (start, end) {
            (Some(s), Some(e))
                if first >= first_usable && last <= last_usable && s < e && e <= disk_len =>
            {
                (s, e)
            }
            _ => {
                return Err(DiskImageError::Corrupt(format!(
                    "GPT entry {index} spans LBA {first}..={last}, outside a {disk_len} byte disk"
                )))
            }
        };
        if partitions
            .iter()
            .any(|p| start < p.start.saturating_add(p.length) && p.start < end)
        {
            return Err(DiskImageError::Corrupt(format!(
                "GPT entry {index} overlaps an earlier partition"
            )));
        }
        partitions.push(Partition {
            index,
            start,
            length: end - start,
            type_label: crate::vhdx::guid_string(&type_guid),
            // The name field is a fixed 72 bytes regardless of entry size.
            name: utf16_name(&entry[56..128]),
            ntfs: false,
            extended: false,
        });
    }
    Ok(Some(Table {
        scheme: Some(PartitionScheme::Gpt),
        partitions,
        truncated,
        incomplete_reason: truncated.then(|| {
            format!(
                "GPT declares {entry_count} entries, exceeding the {} entry budget",
                limits.max_partitions
            )
        }),
    }))
}

fn utf16_name(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .take(36)
        .collect();
    String::from_utf16_lossy(&units)
}

fn read_mbr<R: Read + Seek>(
    r: &mut R,
    disk_len: u64,
    sector: u64,
    limits: &DiskImageLimits,
) -> Result<Table, DiskImageError> {
    if disk_len < 512 {
        return Ok(Table {
            scheme: None,
            partitions: Vec::new(),
            truncated: false,
            incomplete_reason: None,
        });
    }
    let mut boot = [0u8; 512];
    r.seek(SeekFrom::Start(0))?;
    r.read_exact(&mut boot)?;
    if boot[510..512] != MBR_SIGNATURE {
        return Ok(Table {
            scheme: None,
            partitions: Vec::new(),
            truncated: false,
            incomplete_reason: None,
        });
    }
    let mut partitions: Vec<Partition> = Vec::new();
    let mut retained = 0usize;
    let mut truncated = false;
    let mut protective = false;
    let mut has_extended = false;
    for index in 0..4u32 {
        let at = MBR_TABLE_OFFSET + index as usize * MBR_ENTRY_LEN;
        let kind = boot[at + 4];
        if !matches!(boot[at], 0 | 0x80) {
            return Err(DiskImageError::Corrupt(format!(
                "MBR entry {index} has invalid boot indicator 0x{:02x}",
                boot[at]
            )));
        }
        // 0x00 is an unused slot; 0xEE is the protective entry of a GPT disk
        // whose header we already failed to read.
        if kind == 0x00 {
            continue;
        }
        if kind == 0xEE {
            protective = true;
            continue;
        }
        if retained >= limits.max_partitions {
            truncated = true;
            continue;
        }
        let first = u32_at(&boot, at + 8) as u64;
        let sectors = u32_at(&boot, at + 12) as u64;
        if sectors == 0 {
            continue;
        }
        if first == 0 {
            return Err(DiskImageError::Corrupt(format!(
                "MBR entry {index} starts at the partition-table sector"
            )));
        }
        let start = first.checked_mul(sector);
        let length = sectors.checked_mul(sector);
        let (start, length) = match (start, length) {
            (Some(start), Some(length))
                if start < disk_len && start.checked_add(length).is_some_and(|e| e <= disk_len) =>
            {
                (start, length)
            }
            _ => {
                return Err(DiskImageError::Corrupt(format!(
                    "MBR entry {index} spans LBA {first} for {sectors} sectors outside a {disk_len} byte disk"
                )))
            }
        };
        let extended = matches!(kind, 0x05 | 0x0F | 0x85);
        has_extended |= extended;
        let end = start + length;
        if partitions
            .iter()
            .any(|partition| start < partition.start + partition.length && partition.start < end)
        {
            return Err(DiskImageError::Corrupt(format!(
                "MBR entry {index} overlaps an earlier partition"
            )));
        }
        partitions.push(Partition {
            index,
            start,
            length,
            type_label: format!("mbr-{kind:02x}"),
            name: String::new(),
            ntfs: false,
            extended,
        });
        retained += 1;
    }
    let mut reasons = Vec::new();
    if truncated {
        reasons.push(format!(
            "MBR contains more than the {} partition entry budget",
            limits.max_partitions
        ));
    }
    if has_extended {
        reasons.push(
            "extended MBR partitions are reported but logical partitions are not enumerated".into(),
        );
    }
    if protective {
        reasons.push("protective MBR found but both GPT headers are unavailable".into());
    }
    let incomplete_reason = (!reasons.is_empty()).then(|| reasons.join("; "));
    Ok(Table {
        scheme: Some(PartitionScheme::Mbr),
        partitions,
        truncated: incomplete_reason.is_some(),
        incomplete_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{gpt_disk, mbr_disk, PartSpec, MSR_GUID};
    use std::io::{self, Cursor, Read, Seek, SeekFrom};

    const SECTOR: u64 = 512;
    const SECTORS: u64 = 4096;

    fn table(disk: Vec<u8>, limits: &DiskImageLimits) -> Result<Table, DiskImageError> {
        let len = disk.len() as u64;
        read_table(&mut Cursor::new(disk), len, SECTOR as u32, limits)
    }

    fn msr(start_lba: u64, sectors: u64) -> PartSpec {
        PartSpec {
            type_guid: MSR_GUID,
            ..PartSpec::basic(start_lba, sectors, "reserved", false)
        }
    }

    #[test]
    fn gpt_partitions_are_enumerated_and_ntfs_probed() {
        let disk = gpt_disk(
            SECTORS,
            SECTOR,
            &[
                msr(34, 64),
                PartSpec::basic(128, 512, "corpus", true),
                PartSpec::basic(1024, 512, "spillway", false),
            ],
        );
        let table = table(disk, &DiskImageLimits::default()).unwrap();
        assert_eq!(table.scheme, Some(PartitionScheme::Gpt));
        assert!(!table.truncated);
        assert_eq!(table.partitions.len(), 3);
        let ntfs = &table.partitions[1];
        assert_eq!(ntfs.name, "corpus");
        assert_eq!(ntfs.start, 128 * SECTOR);
        assert_eq!(ntfs.length, 512 * SECTOR);
        assert_eq!(ntfs.type_label, "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7");
        assert!(ntfs.ntfs);
        // The reserved partition and the unformatted one carry no NTFS
        // boot record, so neither is opened as a volume.
        assert!(!table.partitions[0].ntfs && !table.partitions[2].ntfs);
        assert_eq!(
            table.partitions[0].type_label,
            "e3c9e316-0b5c-4db8-817d-f92df00215ae"
        );
    }

    #[test]
    fn gpt_corruption_is_a_typed_error() {
        let good = gpt_disk(
            SECTORS,
            SECTOR,
            &[PartSpec::basic(128, 512, "corpus", true)],
        );
        let limits = DiskImageLimits::default();

        let mut damaged = good.clone();
        damaged[SECTOR as usize + 80] ^= 0xFF; // NumberOfPartitionEntries
        disable_backup(&mut damaged);
        match table(damaged, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("checksum"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }

        // A partition claiming to end before it starts.
        let mut damaged = good.clone();
        let entry = (2 * SECTOR) as usize;
        damaged[entry + 40..entry + 48].copy_from_slice(&1u64.to_le_bytes());
        rewrite_array_crc(&mut damaged, SECTOR as usize);
        rewrite_header_crc(&mut damaged, SECTOR as usize);
        disable_backup(&mut damaged);
        match table(damaged, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("before it starts"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }

        // A partition that starts past the end of the disk.
        let mut damaged = good.clone();
        damaged[entry + 32..entry + 40].copy_from_slice(&u64::MAX.to_le_bytes());
        damaged[entry + 40..entry + 48].copy_from_slice(&u64::MAX.to_le_bytes());
        rewrite_array_crc(&mut damaged, SECTOR as usize);
        rewrite_header_crc(&mut damaged, SECTOR as usize);
        disable_backup(&mut damaged);
        match table(damaged, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("outside a"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }

        // An implausible entry size would otherwise size an allocation.
        let mut damaged = good;
        damaged[SECTOR as usize + 84..SECTOR as usize + 88]
            .copy_from_slice(&(1u32 << 20).to_le_bytes());
        rewrite_header_crc(&mut damaged, SECTOR as usize);
        disable_backup(&mut damaged);
        match table(damaged, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("entries are"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }
    }

    /// Recompute the GPT header checksum so a test edits the field it means
    /// to test rather than tripping the checksum first.
    fn rewrite_header_crc(disk: &mut [u8], header_at: usize) {
        disk[header_at + 16..header_at + 20].fill(0);
        let sum = crc32fast::hash(&disk[header_at..header_at + GPT_HEADER_LEN]);
        disk[header_at + 16..header_at + 20].copy_from_slice(&sum.to_le_bytes());
    }

    fn rewrite_array_crc(disk: &mut [u8], header_at: usize) {
        let entry_lba = u64_at(disk, header_at + 72) as usize;
        let count = u32_at(disk, header_at + 80) as usize;
        let len = u32_at(disk, header_at + 84) as usize;
        let start = entry_lba * SECTOR as usize;
        let sum = crc32fast::hash(&disk[start..start + count * len]);
        disk[header_at + 88..header_at + 92].copy_from_slice(&sum.to_le_bytes());
    }

    fn disable_backup(disk: &mut [u8]) {
        let backup = disk.len() - SECTOR as usize;
        disk[backup..backup + 8].fill(0);
    }

    #[test]
    fn a_valid_backup_gpt_recovers_a_damaged_primary() {
        let mut disk = gpt_disk(
            SECTORS,
            SECTOR,
            &[PartSpec::basic(128, 512, "corpus", true)],
        );
        disk[SECTOR as usize + 16] ^= 0xFF;
        let table = table(disk, &DiskImageLimits::default()).unwrap();
        assert_eq!(table.scheme, Some(PartitionScheme::Gpt));
        assert_eq!(table.partitions.len(), 1);
        assert!(table.partitions[0].ntfs);
    }

    #[test]
    fn gpt_entries_are_not_trusted_without_their_array_checksum() {
        let mut disk = gpt_disk(
            SECTORS,
            SECTOR,
            &[PartSpec::basic(128, 512, "corpus", true)],
        );
        disk[(2 * SECTOR + 56) as usize] ^= 1;
        disable_backup(&mut disk);
        match table(disk, &DiskImageLimits::default()) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("array checksum"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }
    }

    #[test]
    fn gpt_headers_and_partition_identities_are_structurally_validated() {
        let limits = DiskImageLimits::default();
        let good = gpt_disk(
            SECTORS,
            SECTOR,
            &[
                PartSpec::basic(128, 128, "one", true),
                PartSpec::basic(256, 128, "two", true),
            ],
        );

        let mut bad_revision = good.clone();
        bad_revision[SECTOR as usize + 8..SECTOR as usize + 12]
            .copy_from_slice(&2u32.to_le_bytes());
        rewrite_header_crc(&mut bad_revision, SECTOR as usize);
        disable_backup(&mut bad_revision);
        match table(bad_revision, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("revision"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }

        let mut zero_disk_guid = good.clone();
        zero_disk_guid[SECTOR as usize + 56..SECTOR as usize + 72].fill(0);
        rewrite_header_crc(&mut zero_disk_guid, SECTOR as usize);
        disable_backup(&mut zero_disk_guid);
        match table(zero_disk_guid, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("disk GUID"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }

        let mut header_overlap = good.clone();
        header_overlap[SECTOR as usize + 72..SECTOR as usize + 80]
            .copy_from_slice(&1u64.to_le_bytes());
        rewrite_header_crc(&mut header_overlap, SECTOR as usize);
        disable_backup(&mut header_overlap);
        match table(header_overlap, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("overlaps a GPT header"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }

        let mut duplicate_guid = good;
        let entries = (2 * SECTOR) as usize;
        let first_guid = duplicate_guid[entries + 16..entries + 32].to_vec();
        duplicate_guid[entries + 128 + 16..entries + 128 + 32].copy_from_slice(&first_guid);
        rewrite_array_crc(&mut duplicate_guid, SECTOR as usize);
        rewrite_header_crc(&mut duplicate_guid, SECTOR as usize);
        disable_backup(&mut duplicate_guid);
        match table(duplicate_guid, &limits) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("duplicate"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }
    }

    #[test]
    fn the_gpt_checksum_budget_returns_no_unverified_partitions() {
        let disk = gpt_disk(
            SECTORS,
            SECTOR,
            &[PartSpec::basic(128, 512, "corpus", true)],
        );
        let limits = DiskImageLimits {
            max_partition_table_bytes: 127,
            ..DiskImageLimits::default()
        };
        let table = table(disk, &limits).unwrap();
        assert!(table.truncated);
        assert!(table.partitions.is_empty());
        assert!(table.incomplete_reason.unwrap().contains("checksum budget"));
    }

    #[test]
    fn the_partition_budget_truncates_rather_than_failing() {
        let disk = gpt_disk(
            SECTORS,
            SECTOR,
            &[
                PartSpec::basic(128, 128, "one", true),
                PartSpec::basic(256, 128, "two", true),
                PartSpec::basic(384, 128, "three", true),
            ],
        );
        let limits = DiskImageLimits {
            max_partitions: 2,
            ..DiskImageLimits::default()
        };
        let table = table(disk, &limits).unwrap();
        assert!(table.truncated);
        assert_eq!(table.partitions.len(), 2);
    }

    #[test]
    fn mbr_is_the_fallback_when_there_is_no_gpt_header() {
        let disk = mbr_disk(
            SECTORS,
            SECTOR,
            &[
                PartSpec::basic(128, 512, "", true),
                PartSpec {
                    mbr_type: 0x05,
                    ..PartSpec::basic(1024, 512, "", false)
                },
            ],
        );
        let table = table(disk, &DiskImageLimits::default()).unwrap();
        assert_eq!(table.scheme, Some(PartitionScheme::Mbr));
        assert_eq!(table.partitions.len(), 2);
        assert_eq!(table.partitions[0].type_label, "mbr-07");
        assert!(table.partitions[0].ntfs);
        // Extended partitions are reported but their logical partitions are
        // not walked, so they are never probed for NTFS.
        assert!(table.partitions[1].extended && !table.partitions[1].ntfs);
        assert!(table.truncated);
        assert!(table
            .incomplete_reason
            .as_deref()
            .unwrap()
            .contains("logical partitions"));
    }

    #[test]
    fn the_mbr_partition_budget_reports_truncation() {
        let disk = mbr_disk(
            SECTORS,
            SECTOR,
            &[
                PartSpec::basic(128, 128, "", true),
                PartSpec::basic(256, 128, "", true),
            ],
        );
        let limits = DiskImageLimits {
            max_partitions: 1,
            ..DiskImageLimits::default()
        };
        let table = table(disk, &limits).unwrap();
        assert!(table.truncated);
        assert_eq!(table.partitions.len(), 1);
        assert!(table.incomplete_reason.unwrap().contains("entry budget"));
    }

    #[test]
    fn overlapping_mbr_partitions_are_rejected() {
        let disk = mbr_disk(
            SECTORS,
            SECTOR,
            &[
                PartSpec::basic(128, 256, "", true),
                PartSpec::basic(256, 256, "", true),
            ],
        );
        match table(disk, &DiskImageLimits::default()) {
            Err(DiskImageError::Corrupt(m)) => assert!(m.contains("overlaps"), "{m}"),
            other => panic!("{other:?}", other = other.map(|t| t.partitions)),
        }
    }

    #[test]
    fn a_disk_with_no_partition_table_yields_no_volumes() {
        let blank = vec![0u8; (SECTORS * SECTOR) as usize];
        let table = table(blank, &DiskImageLimits::default()).unwrap();
        assert_eq!(table.scheme, None);
        assert!(table.partitions.is_empty());
    }

    struct FailAtMbr {
        pos: u64,
        len: u64,
    }

    impl Read for FailAtMbr {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos == 0 {
                return Err(io::Error::other("injected MBR read failure"));
            }
            let wanted = buf.len().min(self.len.saturating_sub(self.pos) as usize);
            buf[..wanted].fill(0);
            self.pos += wanted as u64;
            Ok(wanted)
        }
    }

    impl Seek for FailAtMbr {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(offset) => Some(offset),
                SeekFrom::End(offset) => self.len.checked_add_signed(offset),
                SeekFrom::Current(offset) => self.pos.checked_add_signed(offset),
            }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek overflow"))?;
            Ok(self.pos)
        }
    }

    #[test]
    fn mbr_io_failures_are_not_reported_as_an_empty_disk() {
        let len = SECTORS * SECTOR;
        let error = read_table(
            &mut FailAtMbr { pos: 0, len },
            len,
            SECTOR as u32,
            &DiskImageLimits::default(),
        )
        .err()
        .expect("the injected read failure must propagate");
        assert!(matches!(error, DiskImageError::Io(_)), "{error}");
    }

    #[test]
    fn unsupported_sector_sizes_are_rejected_at_the_public_boundary() {
        let mut disk = Cursor::new(vec![0u8; (SECTORS * SECTOR) as usize]);
        match read_table(
            &mut disk,
            SECTORS * SECTOR,
            513,
            &DiskImageLimits::default(),
        ) {
            Err(DiskImageError::Unsupported(message)) => {
                assert!(message.contains("513"), "{message}")
            }
            other => panic!("{other:?}", other = other.map(|table| table.partitions)),
        }
    }

    #[test]
    fn a_protective_mbr_entry_is_not_mistaken_for_a_partition() {
        // A GPT disk whose header was wiped falls through to the MBR path,
        // where the 0xEE protective entry must not become a volume.
        let mut disk = gpt_disk(
            SECTORS,
            SECTOR,
            &[PartSpec::basic(128, 512, "corpus", true)],
        );
        disk[SECTOR as usize..SECTOR as usize + 8].fill(0);
        disable_backup(&mut disk);
        let table = table(disk, &DiskImageLimits::default()).unwrap();
        assert_eq!(table.scheme, Some(PartitionScheme::Mbr));
        assert!(table.partitions.is_empty());
        assert!(table.truncated);
        assert!(table
            .incomplete_reason
            .unwrap()
            .contains("both GPT headers"));
    }
}
