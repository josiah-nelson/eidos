//! Synthetic, specification-shaped VHDX containers for unit tests.

const MB: u64 = 1024 * 1024;
const VHDX_PARENT_GUID: [u8; 16] =
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
/// 1 MiB blocks over a 2 MiB virtual disk.
pub struct VhdxSpec {
    pub block_size: u32,
    pub virtual_size: u64,
    pub logical_sector_size: u32,
    pub fixed: bool,
    pub differencing: bool,
    pub parent_path: Option<String>,
    pub log_guid: [u8; 16],
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
            log_guid: [0; 16],
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

pub fn build_vhdx(spec: &VhdxSpec) -> Vec<u8> {
    let block_size = u64::from(spec.block_size);
    let data_blocks = spec.virtual_size.div_ceil(block_size).max(1);
    let chunk_ratio = ((1u64 << 23) * u64::from(spec.logical_sector_size)) / block_size;
    let log_offset = MB;
    let bat_offset = 2 * MB;
    let metadata_offset = 3 * MB;
    let payload_offset = 4 * MB;
    let stored_blocks = if spec.fixed {
        data_blocks
    } else {
        (spec.payload.len() as u64).div_ceil(block_size)
    };
    let mut out = vec![0; (payload_offset + stored_blocks * block_size) as usize];

    put(&mut out, 0, b"vhdxfile");
    put(&mut out, 8, &utf16("eidos synthetic fixture"));
    for (index, offset) in [0x1_0000usize, 0x2_0000].into_iter().enumerate() {
        let mut header = vec![0; 4096];
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

    let mut table = vec![0; 64 * 1024];
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
        put(&mut table, at + 28, &1u32.to_le_bytes());
    }
    let sum = crate::vhdx::crc32c(&table);
    put(&mut table, 4, &sum.to_le_bytes());
    put(&mut out, 0x3_0000, &table);
    put(&mut out, 0x4_0000, &table);

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
    let mut metadata = vec![0; MB as usize];
    put(&mut metadata, 0, b"metadata");
    put(&mut metadata, 10, &(items.len() as u16).to_le_bytes());
    let mut item_at = 64 * 1024;
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
            ((file_offset / MB) << 20) | 6
        } else {
            0
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
