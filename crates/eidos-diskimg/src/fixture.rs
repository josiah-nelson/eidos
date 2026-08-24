//! Synthetic disk images for tests and tools: a minimal but specification-
//! shaped VHDX container, and GPT/MBR partitioned disks.
//!
//! These exist so the container, partition, and corruption paths can be
//! exercised from bytes built in code, without shipping binary files and
//! without needing a real filesystem inside. The one test that does need a
//! real NTFS volume uses the compressed image under `tests/fixtures/`, built
//! by `scripts/make-diskimg-fixture.ps1`.

const MB: u64 = 1024 * 1024;

/// GPT type GUID of a Microsoft basic data partition, the type NTFS volumes
/// normally carry.
pub const BASIC_DATA_GUID: [u8; 16] =
    *b"\xA2\xA0\xD0\xEB\xE5\xB9\x33\x44\x87\xC0\x68\xB6\xB7\x26\x99\xC7";
/// GPT type GUID of a Microsoft Reserved partition, which holds no
/// filesystem.
pub const MSR_GUID: [u8; 16] = *b"\x16\xE3\xC9\xE3\x5C\x0B\xB8\x4D\x81\x7D\xF9\x2D\xF0\x02\x15\xAE";
/// Locator type GUID of a VHDX parent image.
pub const VHDX_PARENT_GUID: [u8; 16] =
    *b"\xB7\xEF\x4A\xB0\x9E\xD1\x81\x4A\xB7\x89\x25\xB8\xE9\x44\x59\x13";

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
const VIRTUAL_DISK_ID: [u8; 16] =
    *b"\xAB\x12\xCA\xBE\xE6\xB2\x23\x45\x93\xEF\xC3\x09\xE0\x00\xC7\x46";
const PARENT_LOCATOR: [u8; 16] =
    *b"\x2D\x5F\xD3\xA8\x0B\xB3\x4D\x45\xAB\xF7\xD3\xD8\x48\x34\xAB\x0C";

/// The VHDX to build. Defaults describe the smallest legal dynamic image:
/// 1 MiB blocks (the specification's minimum) over a 2 MiB virtual disk.
pub struct VhdxSpec {
    pub block_size: u32,
    pub virtual_size: u64,
    pub logical_sector_size: u32,
    /// `LeaveBlocksAllocated`: a fixed rather than dynamic payload.
    pub fixed: bool,
    /// `HasParent`: a differencing image.
    pub differencing: bool,
    /// Relative path recorded in the parent locator, when differencing.
    pub parent_path: Option<String>,
    /// Non-zero when the log holds unflushed entries.
    pub log_guid: [u8; 16],
    /// Bytes placed at virtual offset 0.
    pub payload: Vec<u8>,
}

impl Default for VhdxSpec {
    fn default() -> Self {
        Self {
            block_size: MB as u32,
            virtual_size: 2 * MB,
            logical_sector_size: 512,
            fixed: false,
            differencing: false,
            parent_path: None,
            log_guid: [0u8; 16],
            payload: Vec::new(),
        }
    }
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Build a VHDX file whose headers, region table, metadata, and BAT are all
/// internally consistent.
pub fn build_vhdx(spec: &VhdxSpec) -> Vec<u8> {
    let block_size = spec.block_size as u64;
    let data_blocks = spec.virtual_size.div_ceil(block_size).max(1);
    let chunk_ratio = ((1u64 << 23) * spec.logical_sector_size as u64) / block_size;
    let log_offset = MB;
    let bat_offset = 2 * MB;
    let metadata_offset = 3 * MB;
    let payload_offset = 4 * MB;
    let stored_blocks = if spec.fixed {
        data_blocks
    } else {
        (spec.payload.len() as u64).div_ceil(block_size)
    };
    let mut out = vec![0u8; (payload_offset + stored_blocks * block_size) as usize];

    put(&mut out, 0, b"vhdxfile");
    put(&mut out, 8, &utf16("eidos synthetic fixture"));

    // Both header copies; the second wins on sequence number.
    for (index, offset) in [0x1_0000usize, 0x2_0000].into_iter().enumerate() {
        let mut header = vec![0u8; 4096];
        put(&mut header, 0, b"head");
        put(&mut header, 8, &(index as u64 + 1).to_le_bytes());
        put(&mut header, 48, &spec.log_guid);
        put(&mut header, 64, &0u16.to_le_bytes()); // LogVersion
        put(&mut header, 66, &1u16.to_le_bytes()); // Version
        put(&mut header, 68, &(MB as u32).to_le_bytes());
        put(&mut header, 72, &log_offset.to_le_bytes());
        let sum = crate::vhdx::crc32c(&header);
        put(&mut header, 4, &sum.to_le_bytes());
        put(&mut out, offset, &header);
    }

    let mut table = vec![0u8; 64 * 1024];
    put(&mut table, 0, b"regi");
    put(&mut table, 8, &2u32.to_le_bytes());
    for (index, (guid, offset)) in [(BAT_REGION, bat_offset), (METADATA_REGION, metadata_offset)]
        .into_iter()
        .enumerate()
    {
        let at = 16 + index * 32;
        put(&mut table, at, &guid);
        put(&mut table, at + 16, &offset.to_le_bytes());
        put(&mut table, at + 24, &(MB as u32).to_le_bytes());
        put(&mut table, at + 28, &1u32.to_le_bytes()); // Required
    }
    let sum = crate::vhdx::crc32c(&table);
    put(&mut table, 4, &sum.to_le_bytes());
    put(&mut out, 0x3_0000, &table);
    put(&mut out, 0x4_0000, &table);

    // Metadata region: table header, entries, then item payloads at 64 KiB.
    let mut items: Vec<([u8; 16], Vec<u8>)> = vec![
        (FILE_PARAMETERS, {
            let flags = u32::from(spec.fixed) | (u32::from(spec.differencing) << 1);
            [spec.block_size.to_le_bytes(), flags.to_le_bytes()].concat()
        }),
        (VIRTUAL_DISK_SIZE, spec.virtual_size.to_le_bytes().to_vec()),
        (
            LOGICAL_SECTOR_SIZE,
            spec.logical_sector_size.to_le_bytes().to_vec(),
        ),
        (
            PHYSICAL_SECTOR_SIZE,
            spec.logical_sector_size.to_le_bytes().to_vec(),
        ),
        (VIRTUAL_DISK_ID, vec![1; 16]),
    ];
    if let Some(path) = &spec.parent_path {
        let pairs = [
            (
                utf16("parent_linkage"),
                utf16("{00000000-0000-0000-0000-000000000001}"),
            ),
            (utf16("relative_path"), utf16(path)),
        ];
        let mut locator = Vec::new();
        locator.extend_from_slice(&VHDX_PARENT_GUID);
        locator.extend_from_slice(&0u16.to_le_bytes()); // Reserved
        locator.extend_from_slice(&(pairs.len() as u16).to_le_bytes());
        let data_offset = 20 + pairs.len() * 12;
        let mut data = Vec::new();
        for (key, value) in &pairs {
            let key_offset = data_offset + data.len();
            data.extend_from_slice(key);
            let value_offset = data_offset + data.len();
            data.extend_from_slice(value);
            locator.extend_from_slice(&(key_offset as u32).to_le_bytes());
            locator.extend_from_slice(&(value_offset as u32).to_le_bytes());
            locator.extend_from_slice(&(key.len() as u16).to_le_bytes());
            locator.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        locator.append(&mut data);
        items.push((PARENT_LOCATOR, locator));
    }
    let mut metadata = vec![0u8; MB as usize];
    put(&mut metadata, 0, b"metadata");
    put(&mut metadata, 10, &(items.len() as u16).to_le_bytes());
    let mut item_at = 64 * 1024usize;
    for (index, (guid, payload)) in items.iter().enumerate() {
        let at = 32 + index * 32;
        put(&mut metadata, at, guid);
        put(&mut metadata, at + 16, &(item_at as u32).to_le_bytes());
        put(
            &mut metadata,
            at + 20,
            &(payload.len() as u32).to_le_bytes(),
        );
        let is_virtual_disk = !matches!(*guid, FILE_PARAMETERS | PARENT_LOCATOR);
        let flags = 0b100 | (u32::from(is_virtual_disk) << 1);
        put(&mut metadata, at + 24, &flags.to_le_bytes());
        put(&mut metadata, item_at, payload);
        item_at += payload.len().next_multiple_of(8);
    }
    put(&mut out, metadata_offset as usize, &metadata);

    for block in 0..data_blocks {
        let index = block + block / chunk_ratio;
        let entry = if block < stored_blocks {
            let file_offset = payload_offset + block * block_size;
            ((file_offset / MB) << 20) | 6 // PAYLOAD_BLOCK_FULLY_PRESENT
        } else {
            0 // PAYLOAD_BLOCK_NOT_PRESENT: reads as zero
        };
        put(
            &mut out,
            (bat_offset + index * 8) as usize,
            &entry.to_le_bytes(),
        );
    }
    put(&mut out, payload_offset as usize, &spec.payload);
    out
}

/// A partition to place in a synthetic partition table.
pub struct PartSpec {
    pub type_guid: [u8; 16],
    /// MBR partition type byte, used only by [`mbr_disk`].
    pub mbr_type: u8,
    pub start_lba: u64,
    pub sectors: u64,
    pub name: String,
    /// Write an NTFS boot signature at the partition's first sector.
    pub ntfs: bool,
}

impl PartSpec {
    pub fn basic(start_lba: u64, sectors: u64, name: &str, ntfs: bool) -> Self {
        Self {
            type_guid: BASIC_DATA_GUID,
            mbr_type: 0x07,
            start_lba,
            sectors,
            name: name.to_string(),
            ntfs,
        }
    }
}

fn write_boot_signature(disk: &mut [u8], at: usize) {
    if at + 512 <= disk.len() {
        put(disk, at + 3, b"NTFS    ");
        put(disk, at + 510, &[0x55, 0xAA]);
    }
}

/// A GPT-partitioned disk of `sectors` sectors, with the entry array at LBA 2.
pub fn gpt_disk(sectors: u64, sector: u64, parts: &[PartSpec]) -> Vec<u8> {
    let mut disk = vec![0u8; (sectors * sector) as usize];
    // Protective MBR.
    disk[0x1BE + 4] = 0xEE;
    put(&mut disk, 510, &[0x55, 0xAA]);

    let entry_count = parts.len().max(4) as u32;
    let mut array = vec![0u8; entry_count as usize * 128];
    for (index, part) in parts.iter().enumerate() {
        let at = index * 128;
        put(&mut array, at, &part.type_guid);
        put(&mut array, at + 16, &[index as u8 + 1; 16]);
        put(&mut array, at + 32, &part.start_lba.to_le_bytes());
        put(
            &mut array,
            at + 40,
            &(part.start_lba + part.sectors - 1).to_le_bytes(),
        );
        let name = utf16(&part.name);
        put(&mut array, at + 56, &name[..name.len().min(72)]);
        if part.ntfs {
            write_boot_signature(&mut disk, (part.start_lba * sector) as usize);
        }
    }
    let primary_array_lba = 2;
    let array_sectors = (array.len() as u64).div_ceil(sector);
    let backup_header_lba = sectors - 1;
    let backup_array_lba = backup_header_lba - array_sectors;
    put(&mut disk, (primary_array_lba * sector) as usize, &array);
    put(&mut disk, (backup_array_lba * sector) as usize, &array);

    let make_header = |my_lba: u64, alternate_lba: u64, entry_lba: u64| {
        let mut header = vec![0u8; sector as usize];
        put(&mut header, 0, b"EFI PART");
        put(&mut header, 8, &0x0001_0000u32.to_le_bytes());
        put(&mut header, 12, &92u32.to_le_bytes());
        put(&mut header, 24, &my_lba.to_le_bytes());
        put(&mut header, 32, &alternate_lba.to_le_bytes());
        put(&mut header, 40, &34u64.to_le_bytes());
        put(&mut header, 48, &(sectors - 34).to_le_bytes());
        put(&mut header, 56, &[0xA5; 16]);
        put(&mut header, 72, &entry_lba.to_le_bytes());
        put(&mut header, 80, &entry_count.to_le_bytes());
        put(&mut header, 84, &128u32.to_le_bytes());
        put(&mut header, 88, &crc32fast::hash(&array).to_le_bytes());
        let sum = crc32fast::hash(&header[..92]);
        put(&mut header, 16, &sum.to_le_bytes());
        header
    };
    let primary = make_header(1, backup_header_lba, primary_array_lba);
    let backup = make_header(backup_header_lba, 1, backup_array_lba);
    put(&mut disk, sector as usize, &primary);
    put(&mut disk, (backup_header_lba * sector) as usize, &backup);
    disk
}

/// An MBR-partitioned disk of `sectors` sectors.
pub fn mbr_disk(sectors: u64, sector: u64, parts: &[PartSpec]) -> Vec<u8> {
    let mut disk = vec![0u8; (sectors * sector) as usize];
    put(&mut disk, 510, &[0x55, 0xAA]);
    for (index, part) in parts.iter().take(4).enumerate() {
        let at = 0x1BE + index * 16;
        disk[at + 4] = part.mbr_type;
        put(&mut disk, at + 8, &(part.start_lba as u32).to_le_bytes());
        put(&mut disk, at + 12, &(part.sectors as u32).to_le_bytes());
        if part.ntfs {
            write_boot_signature(&mut disk, (part.start_lba * sector) as usize);
        }
    }
    disk
}
