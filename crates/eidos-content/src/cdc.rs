//! Content-defined chunking (FastCDC with normalized chunking) and a text
//! heuristic. Shared by the observatory's content economics probe and the
//! fleet's content-transfer bakeoff; platform-neutral so chunk stability
//! under edits is tested directly.

/// Gear table derived from a fixed seed so chunk boundaries are identical
/// on every host and build.
fn gear() -> &'static [u64; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u64; 256];
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"eidos-observe-fastcdc-gear/1");
        let mut reader = hasher.finalize_xof();
        let mut bytes = [0u8; 256 * 8];
        reader.fill(&mut bytes);
        for (index, slot) in table.iter_mut().enumerate() {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
            *slot = u64::from_le_bytes(word);
        }
        table
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CdcParams {
    pub min: usize,
    pub average: usize,
    pub max: usize,
}

impl CdcParams {
    pub const DEFAULT: CdcParams = CdcParams {
        min: 4 * 1024,
        average: 16 * 1024,
        max: 64 * 1024,
    };

    fn masks(&self) -> (u64, u64) {
        let bits = (self.average.max(2) as f64).log2().round() as u32;
        // Normalization level 1: stricter before the average, looser after.
        let small = (1u64 << (bits + 1)) - 1;
        let large = (1u64 << (bits - 1)) - 1;
        (small, large)
    }
}

/// Streaming chunker: feed bytes in any pieces, collect chunk lengths.
pub struct Chunker {
    params: CdcParams,
    mask_small: u64,
    mask_large: u64,
    hash: u64,
    length: usize,
    pub chunks: Vec<usize>,
}

impl Chunker {
    pub fn new(params: CdcParams) -> Self {
        let (mask_small, mask_large) = params.masks();
        Self {
            params,
            mask_small,
            mask_large,
            hash: 0,
            length: 0,
            chunks: Vec::new(),
        }
    }

    /// Returns the boundaries (as chunk lengths) closed while consuming
    /// `bytes`, so a caller can hash chunk contents alongside.
    pub fn update(&mut self, bytes: &[u8], mut on_boundary: impl FnMut(usize)) {
        let gear = gear();
        for byte in bytes {
            self.length += 1;
            if self.length < self.params.min {
                continue;
            }
            self.hash = (self.hash << 1).wrapping_add(gear[*byte as usize]);
            let mask = if self.length < self.params.average {
                self.mask_small
            } else {
                self.mask_large
            };
            if self.hash & mask == 0 || self.length >= self.params.max {
                self.chunks.push(self.length);
                on_boundary(self.length);
                self.hash = 0;
                self.length = 0;
            }
        }
    }

    pub fn finish(&mut self) -> Option<usize> {
        if self.length > 0 {
            let last = self.length;
            self.chunks.push(last);
            self.length = 0;
            self.hash = 0;
            Some(last)
        } else {
            None
        }
    }
}

/// Text-likeness of a sample: no NUL bytes and at least 90 % of bytes are
/// printable ASCII, whitespace, or UTF-8 continuation/lead bytes.
pub fn looks_textual(sample: &[u8]) -> Option<bool> {
    if sample.is_empty() {
        return None;
    }
    if sample.contains(&0) {
        return Some(false);
    }
    let printable = sample
        .iter()
        .filter(|b| matches!(b, 0x09 | 0x0a | 0x0d | 0x20..=0x7e | 0x80..=0xff))
        .count();
    Some(printable * 10 >= sample.len() * 9)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, seed: u8) -> Vec<u8> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[seed]);
        let mut out = vec![0u8; len];
        hasher.finalize_xof().fill(&mut out);
        out
    }

    fn chunk_hashes(data: &[u8]) -> Vec<blake3::Hash> {
        let mut chunker = Chunker::new(CdcParams::DEFAULT);
        let mut hashes = Vec::new();
        let mut start = 0;
        chunker.update(data, |length| {
            hashes.push(blake3::hash(&data[start..start + length]));
            start += length;
        });
        if chunker.finish().is_some() {
            hashes.push(blake3::hash(&data[start..]));
        }
        hashes
    }

    #[test]
    fn chunks_respect_bounds_and_average() {
        let data = pseudo_random(4 << 20, 1);
        let mut chunker = Chunker::new(CdcParams::DEFAULT);
        chunker.update(&data, |_| {});
        chunker.finish();
        let chunks = &chunker.chunks;
        assert_eq!(chunks.iter().sum::<usize>(), data.len());
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(*chunk >= CdcParams::DEFAULT.min && *chunk <= CdcParams::DEFAULT.max);
        }
        let average = data.len() / chunks.len();
        assert!(
            (8 * 1024..=32 * 1024).contains(&average),
            "average {average}"
        );
    }

    #[test]
    fn an_insertion_near_the_start_keeps_most_chunks() {
        let original = pseudo_random(2 << 20, 2);
        let mut edited = original[..1000].to_vec();
        edited.extend_from_slice(b"inserted text that shifts everything after it");
        edited.extend_from_slice(&original[1000..]);
        let before = chunk_hashes(&original);
        let after = chunk_hashes(&edited);
        let reused = after.iter().filter(|h| before.contains(h)).count();
        assert!(
            reused * 10 >= before.len() * 8,
            "reused {reused} of {}",
            before.len()
        );
    }

    #[test]
    fn streaming_in_pieces_matches_one_shot() {
        let data = pseudo_random(1 << 20, 3);
        let mut whole = Chunker::new(CdcParams::DEFAULT);
        whole.update(&data, |_| {});
        whole.finish();
        let mut pieces = Chunker::new(CdcParams::DEFAULT);
        for piece in data.chunks(7_777) {
            pieces.update(piece, |_| {});
        }
        pieces.finish();
        assert_eq!(whole.chunks, pieces.chunks);
    }

    #[test]
    fn text_heuristic() {
        assert_eq!(looks_textual(b"fn main() {}\n"), Some(true));
        assert_eq!(looks_textual(b"\x00\x01\x02"), Some(false));
        assert_eq!(looks_textual(&[]), None);
        assert_eq!(looks_textual(&[0x01; 100]), Some(false));
    }
}
