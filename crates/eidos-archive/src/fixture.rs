//! Synthetic ZIP builder for tests and tools: stored members, optional
//! ZIP64 end records, per-member flags and extra fields. Not a general
//! writer — it exists so fixtures can exercise the parser without shipping
//! binary files.

/// Minimal ZIP writer for fixtures: stored members, optional ZIP64 end
/// records, optional per-member extra fields and flags.
pub struct Entry {
    pub name: Vec<u8>,
    pub data: Vec<u8>,
    pub flags: u16,
    pub extra: Vec<u8>,
    pub external: u32,
    pub size_override: Option<(u32, u32)>,
}

impl Entry {
    pub fn file(name: &str, data: &[u8]) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            data: data.to_vec(),
            flags: 0,
            extra: Vec::new(),
            external: 0,
            size_override: None,
        }
    }
    pub fn dir(name: &str) -> Self {
        let mut e = Self::file(name, b"");
        e.external = 0x10;
        e
    }
}

pub fn build(entries: &[Entry], comment: &[u8], zip64: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for e in entries {
        offsets.push(out.len() as u32);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&e.flags.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // stored
        out.extend_from_slice(&0x6000u16.to_le_bytes()); // 12:00:00
        out.extend_from_slice(&0x5D15u16.to_le_bytes()); // 2026-08-21
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&e.data);
    }
    let cd_start = out.len();
    for (e, off) in entries.iter().zip(&offsets) {
        let (size, comp) = e
            .size_override
            .unwrap_or((e.data.len() as u32, e.data.len() as u32));
        out.extend_from_slice(&crate::zip::SIG_CENTRAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&e.flags.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x6000u16.to_le_bytes());
        out.extend_from_slice(&0x5D15u16.to_le_bytes());
        out.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        out.extend_from_slice(&comp.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(e.extra.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&e.external.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&e.extra);
    }
    let cd_size = (out.len() - cd_start) as u64;
    if zip64 {
        let eocd64_pos = out.len() as u64;
        out.extend_from_slice(&crate::zip::SIG_EOCD64.to_le_bytes());
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&(cd_start as u64).to_le_bytes());
        out.extend_from_slice(&crate::zip::SIG_EOCD64_LOCATOR.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&eocd64_pos.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
    }
    out.extend_from_slice(&crate::zip::SIG_EOCD.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    let n16: u16 = if zip64 { 0xFFFF } else { entries.len() as u16 };
    out.extend_from_slice(&n16.to_le_bytes());
    out.extend_from_slice(&n16.to_le_bytes());
    let (s32, o32) = if zip64 {
        (0xFFFF_FFFFu32, 0xFFFF_FFFFu32)
    } else {
        (cd_size as u32, cd_start as u32)
    };
    out.extend_from_slice(&s32.to_le_bytes());
    out.extend_from_slice(&o32.to_le_bytes());
    out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    out.extend_from_slice(comment);
    out
}
