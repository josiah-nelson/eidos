//! Line-aware streaming chunker with exact byte and line ranges.
//!
//! Bytes are split on newline code units *in the byte domain* for the
//! detected encoding (so chunk boundaries are exact file offsets), then each
//! line is decoded. Over-long lines are split at a bounded length so memory
//! stays bounded regardless of input.

use crate::sniff::Encoding;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerConfig {
    /// Emit a chunk once its decoded text reaches this many bytes.
    pub target_bytes: usize,
    /// A single line longer than this (in input bytes) is split.
    pub max_line_bytes: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            target_bytes: 16 * 1024,
            max_line_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub ordinal: u32,
    /// Absolute input byte range `[byte_start, byte_end)`.
    pub byte_start: u64,
    pub byte_end: u64,
    /// Zero-based line index range (inclusive) covered by this chunk.
    pub line_start: u64,
    pub line_end: u64,
    /// Decoded text (invalid sequences replaced with U+FFFD).
    pub text: String,
    /// True when the chunk ends inside a line that was force-split.
    pub split_line: bool,
}

pub struct Chunker {
    encoding: Encoding,
    cfg: ChunkerConfig,
    buf: Vec<u8>,
    /// Absolute offset of `buf[0]`.
    buf_offset: u64,
    line: u64,
    ordinal: u32,
    cur_text: String,
    cur_start: u64,
    cur_line_start: u64,
    cur_last_line: u64,
    cur_split: bool,
    cur_has_data: bool,
    pub lines_total: u64,
    pub chars_total: u64,
}

impl Chunker {
    /// `start_offset` is the absolute offset of the first byte that will be
    /// pushed (i.e. the BOM length).
    pub fn new(encoding: Encoding, start_offset: u64, cfg: ChunkerConfig) -> Self {
        Self {
            encoding,
            cfg,
            buf: Vec::with_capacity(64 * 1024),
            buf_offset: start_offset,
            line: 0,
            ordinal: 0,
            cur_text: String::with_capacity(cfg.target_bytes + 1024),
            cur_start: start_offset,
            cur_line_start: 0,
            cur_last_line: 0,
            cur_split: false,
            cur_has_data: false,
            lines_total: 0,
            chars_total: 0,
        }
    }

    /// Feed input bytes; completed chunks are appended to `out`.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<Chunk>) {
        self.buf.extend_from_slice(data);
        self.drain(false, out);
    }

    /// Flush everything remaining.
    pub fn finish(&mut self, out: &mut Vec<Chunk>) {
        self.drain(true, out);
        if self.cur_has_data {
            self.emit(out, false);
        }
    }

    fn find_newline(&self, from: usize) -> Option<usize> {
        match self.encoding {
            Encoding::Utf8 | Encoding::Windows1252 => {
                memchr::memchr(b'\n', &self.buf[from..]).map(|i| from + i)
            }
            Encoding::Utf16Le => {
                let mut i = from;
                // Keep code-unit alignment relative to the stream start.
                if (self.buf_offset as usize + i) % 2 == 1 {
                    i += 1;
                }
                while i + 1 < self.buf.len() {
                    if self.buf[i] == b'\n' && self.buf[i + 1] == 0 {
                        return Some(i + 1);
                    }
                    i += 2;
                }
                None
            }
            Encoding::Utf16Be => {
                let mut i = from;
                if (self.buf_offset as usize + i) % 2 == 1 {
                    i += 1;
                }
                while i + 1 < self.buf.len() {
                    if self.buf[i] == 0 && self.buf[i + 1] == b'\n' {
                        return Some(i + 1);
                    }
                    i += 2;
                }
                None
            }
        }
    }

    fn drain(&mut self, eof: bool, out: &mut Vec<Chunk>) {
        let mut pos = 0usize;
        loop {
            let remaining = self.buf.len() - pos;
            if remaining == 0 {
                break;
            }
            match self.find_newline(pos) {
                Some(nl) => {
                    let end = nl + 1;
                    self.take_segment(pos, end, true, out);
                    pos = end;
                }
                None => {
                    if remaining >= self.cfg.max_line_bytes {
                        // Force-split a very long line at a safe boundary.
                        let mut end = pos + self.cfg.max_line_bytes;
                        end = self.safe_boundary(end);
                        if end <= pos {
                            break;
                        }
                        self.take_segment(pos, end, false, out);
                        pos = end;
                        continue;
                    }
                    if eof {
                        let end = self.buf.len();
                        self.take_segment(pos, end, false, out);
                        pos = end;
                        // A final line without a newline still counts as a line.
                        self.lines_total += 1;
                        self.line += 1;
                    }
                    break;
                }
            }
        }
        if pos > 0 {
            self.buf.drain(..pos);
            self.buf_offset += pos as u64;
        }
    }

    /// Back `end` off to a code-unit / UTF-8 character boundary.
    fn safe_boundary(&self, mut end: usize) -> usize {
        match self.encoding {
            Encoding::Utf16Le | Encoding::Utf16Be => {
                if (self.buf_offset as usize + end) % 2 == 1 {
                    end -= 1;
                }
                // Avoid splitting a surrogate pair.
                if end >= 2 {
                    let (a, b) = (self.buf[end - 2], self.buf[end - 1]);
                    let unit = match self.encoding {
                        Encoding::Utf16Le => u16::from_le_bytes([a, b]),
                        _ => u16::from_be_bytes([a, b]),
                    };
                    if (0xD800..0xDC00).contains(&unit) {
                        end -= 2;
                    }
                }
                end
            }
            Encoding::Utf8 => utf8_boundary(&self.buf, end),
            Encoding::Windows1252 => end,
        }
    }

    fn take_segment(&mut self, start: usize, end: usize, is_line_end: bool, out: &mut Vec<Chunk>) {
        debug_assert!(
            self.encoding != Encoding::Utf8
                || end == self.buf.len()
                || (self.buf[end] & 0xC0) != 0x80,
            "segment end {end} is not a UTF-8 boundary"
        );
        let bytes = &self.buf[start..end];
        let (decoded, _, _) = self.encoding.rs().decode(bytes);
        // `sniff` already consumed any BOM, so the decoder never sees one here.
        let text: &str = &decoded;
        if !self.cur_has_data {
            self.cur_start = self.buf_offset + start as u64;
            self.cur_line_start = self.line;
            self.cur_has_data = true;
        }
        self.cur_text.push_str(text);
        self.chars_total += text.chars().count() as u64;
        self.cur_last_line = self.line;
        if is_line_end {
            self.line += 1;
            self.lines_total += 1;
            self.cur_split = false;
        } else {
            self.cur_split = true;
        }
        let cur_end = self.buf_offset + end as u64;
        if self.cur_text.len() >= self.cfg.target_bytes {
            self.emit_at(out, cur_end, !is_line_end);
        }
    }

    fn emit(&mut self, out: &mut Vec<Chunk>, split: bool) {
        let end = self.buf_offset;
        self.emit_at(out, end, split);
    }

    fn emit_at(&mut self, out: &mut Vec<Chunk>, byte_end: u64, split: bool) {
        let text = std::mem::take(&mut self.cur_text);
        out.push(Chunk {
            ordinal: self.ordinal,
            byte_start: self.cur_start,
            byte_end,
            line_start: self.cur_line_start,
            line_end: self.cur_last_line,
            text,
            split_line: split || self.cur_split,
        });
        self.ordinal += 1;
        self.cur_has_data = false;
        self.cur_split = false;
        self.cur_text = String::with_capacity(self.cfg.target_bytes + 1024);
    }
}

#[inline]
fn is_continuation(b: u8) -> bool {
    (b & 0xC0) == 0x80
}

/// Length of the UTF-8 sequence introduced by `lead` (1 for ASCII and for
/// invalid lead bytes, which the decoder will replace).
#[inline]
fn sequence_len(lead: u8) -> usize {
    if lead >= 0xF0 {
        4
    } else if lead >= 0xE0 {
        3
    } else if lead >= 0xC0 {
        2
    } else {
        1
    }
}

/// Largest split position `<= end` that does not fall inside a UTF-8
/// sequence. A position is a boundary when the byte *at* it is not a
/// continuation byte. When `end` reaches the end of the buffer, the last
/// (possibly incomplete) sequence is inspected so a chunk never ends halfway
/// through a character whose remaining bytes have not been pushed yet.
fn utf8_boundary(buf: &[u8], end: usize) -> usize {
    let mut e = end.min(buf.len());
    if e < buf.len() {
        let floor = e.saturating_sub(3);
        while e > floor && is_continuation(buf[e]) {
            e -= 1;
        }
        return e;
    }
    // `e == buf.len()`: find the last lead byte within the final 4 bytes.
    let floor = e.saturating_sub(4);
    let mut k = e;
    while k > floor {
        k -= 1;
        if !is_continuation(buf[k]) {
            if k + sequence_len(buf[k]) > e {
                return k;
            }
            return e;
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        enc: Encoding,
        start: u64,
        cfg: ChunkerConfig,
        data: &[u8],
        block: usize,
    ) -> (Vec<Chunk>, u64) {
        let mut c = Chunker::new(enc, start, cfg);
        let mut out = Vec::new();
        for b in data.chunks(block.max(1)) {
            c.push(b, &mut out);
        }
        c.finish(&mut out);
        (out, c.lines_total)
    }

    #[test]
    fn utf8_exact_ranges() {
        let data = b"first line\nsecond\nthird line here\nlast";
        let cfg = ChunkerConfig {
            target_bytes: 12,
            max_line_bytes: 1024,
        };
        let (chunks, lines) = run(Encoding::Utf8, 0, cfg, data, 5);
        assert_eq!(lines, 4);
        // Contiguous coverage.
        assert_eq!(chunks[0].byte_start, 0);
        for w in chunks.windows(2) {
            assert_eq!(w[0].byte_end, w[1].byte_start);
        }
        assert_eq!(chunks.last().unwrap().byte_end, data.len() as u64);
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, std::str::from_utf8(data).unwrap());
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks.last().unwrap().line_end, 3);
        // Each chunk's bytes decode to its text.
        for c in &chunks {
            assert_eq!(
                std::str::from_utf8(&data[c.byte_start as usize..c.byte_end as usize]).unwrap(),
                c.text
            );
        }
    }

    #[test]
    fn utf16le_with_bom_offset() {
        let text = "alpha\r\nbeta\r\ngamma";
        let mut data = vec![0xFF, 0xFE];
        data.extend(text.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let cfg = ChunkerConfig {
            target_bytes: 8,
            max_line_bytes: 1024,
        };
        let (chunks, lines) = run(Encoding::Utf16Le, 2, cfg, &data[2..], 3);
        assert_eq!(lines, 3);
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, text);
        assert_eq!(chunks[0].byte_start, 2);
        assert_eq!(chunks.last().unwrap().byte_end, data.len() as u64);
    }

    #[test]
    fn long_line_is_split_with_bounded_memory() {
        let data = vec![b'x'; 10_000];
        let cfg = ChunkerConfig {
            target_bytes: 1000,
            max_line_bytes: 1024,
        };
        let (chunks, lines) = run(Encoding::Utf8, 0, cfg, &data, 777);
        assert_eq!(lines, 1);
        assert!(chunks.len() >= 9);
        assert!(chunks.iter().all(|c| c.text.len() <= 1024));
        assert!(chunks[..chunks.len() - 1].iter().all(|c| c.split_line));
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined.len(), 10_000);
        for w in chunks.windows(2) {
            assert_eq!(w[0].byte_end, w[1].byte_start);
        }
    }

    #[test]
    fn utf8_multibyte_not_split() {
        let s = "é".repeat(3000); // 6000 bytes, 2 per char
        let cfg = ChunkerConfig {
            target_bytes: 500,
            max_line_bytes: 1001, // odd to force boundary adjustment
        };
        for block in [333usize, 1001, 1000, 7, 6000] {
            let (chunks, _) = run(Encoding::Utf8, 0, cfg, s.as_bytes(), block);
            let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(joined, s, "block {block}");
            assert!(!joined.contains('\u{FFFD}'), "block {block}");
            for w in chunks.windows(2) {
                assert_eq!(w[0].byte_end, w[1].byte_start);
            }
            for c in &chunks {
                assert_eq!(&s[c.byte_start as usize..c.byte_end as usize], c.text);
            }
        }
    }

    #[test]
    fn utf8_four_byte_sequences_and_mixed_widths() {
        // 1-, 2-, 3-, and 4-byte characters with no newline at all.
        let unit = "a€😀é";
        let s = unit.repeat(800);
        let cfg = ChunkerConfig {
            target_bytes: 300,
            max_line_bytes: 257,
        };
        for block in [1usize, 2, 3, 5, 257, 258, 1024] {
            let (chunks, _) = run(Encoding::Utf8, 0, cfg, s.as_bytes(), block);
            let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(joined, s, "block {block}");
            assert!(chunks.iter().all(|c| c.text.len() <= 300 + 257));
        }
    }

    #[test]
    fn utf8_boundary_rules() {
        let b = "é".as_bytes(); // C3 A9
        assert_eq!(utf8_boundary(b, 1), 0, "inside a complete sequence");
        assert_eq!(utf8_boundary(b, 2), 2);
        assert_eq!(utf8_boundary(&b[..1], 1), 0, "buffer ends mid-sequence");
        let ascii = b"abc";
        assert_eq!(utf8_boundary(ascii, 2), 2);
        assert_eq!(utf8_boundary(ascii, 3), 3);
        let smile = "😀".as_bytes(); // F0 9F 98 80
        for cut in 1..4 {
            assert_eq!(utf8_boundary(smile, cut), 0, "cut {cut}");
            assert_eq!(utf8_boundary(&smile[..cut], cut), 0, "partial {cut}");
        }
        assert_eq!(utf8_boundary(smile, 4), 4);
    }

    #[test]
    fn windows_1252_decodes() {
        let data = b"caf\xe9\nna\xefve\n";
        let (chunks, lines) = run(Encoding::Windows1252, 0, ChunkerConfig::default(), data, 4);
        assert_eq!(lines, 2);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "café\nnaïve\n");
        assert_eq!(chunks[0].line_end, 1);
    }
}
