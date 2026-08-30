//! Bounded encoding detection and binary rejection.
//!
//! Only the first few KiB are examined. The decision is deterministic and
//! recorded with the content record so it can be explained and revisited.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Windows1252 => "windows-1252",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "utf-8" => Encoding::Utf8,
            "utf-16le" => Encoding::Utf16Le,
            "utf-16be" => Encoding::Utf16Be,
            "windows-1252" => Encoding::Windows1252,
            _ => return None,
        })
    }

    pub fn rs(self) -> &'static encoding_rs::Encoding {
        match self {
            Encoding::Utf8 => encoding_rs::UTF_8,
            Encoding::Utf16Le => encoding_rs::UTF_16LE,
            Encoding::Utf16Be => encoding_rs::UTF_16BE,
            Encoding::Windows1252 => encoding_rs::WINDOWS_1252,
        }
    }

    /// Code unit size in bytes (line splitting happens in the byte domain).
    pub fn unit(self) -> usize {
        match self {
            Encoding::Utf16Le | Encoding::Utf16Be => 2,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sniff {
    Text {
        encoding: Encoding,
        /// Bytes of byte-order mark to skip.
        bom_len: usize,
    },
    Binary {
        reason: &'static str,
    },
}

/// Decide whether `head` (the first bytes of a file) is literal text.
pub fn sniff(head: &[u8]) -> Sniff {
    if head.is_empty() {
        return Sniff::Text {
            encoding: Encoding::Utf8,
            bom_len: 0,
        };
    }
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Sniff::Text {
            encoding: Encoding::Utf8,
            bom_len: 3,
        };
    }
    if head.starts_with(&[0xFF, 0xFE]) {
        return Sniff::Text {
            encoding: Encoding::Utf16Le,
            bom_len: 2,
        };
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        return Sniff::Text {
            encoding: Encoding::Utf16Be,
            bom_len: 2,
        };
    }
    let nul_total = head.iter().filter(|&&b| b == 0).count();
    if nul_total > 0 {
        // UTF-16 without BOM: NULs concentrated on one parity.
        let (mut even_nul, mut odd_nul) = (0usize, 0usize);
        for (i, &b) in head.iter().enumerate() {
            if b == 0 {
                if i % 2 == 0 {
                    even_nul += 1;
                } else {
                    odd_nul += 1;
                }
            }
        }
        let pairs = head.len() / 2;
        if pairs >= 4 {
            let odd_ratio = odd_nul as f64 / pairs as f64;
            let even_ratio = even_nul as f64 / pairs as f64;
            if odd_ratio >= 0.4 && even_ratio <= 0.05 && looks_textual_utf16(head, true) {
                return Sniff::Text {
                    encoding: Encoding::Utf16Le,
                    bom_len: 0,
                };
            }
            if even_ratio >= 0.4 && odd_ratio <= 0.05 && looks_textual_utf16(head, false) {
                return Sniff::Text {
                    encoding: Encoding::Utf16Be,
                    bom_len: 0,
                };
            }
        }
        return Sniff::Binary {
            reason: "contains NUL bytes",
        };
    }
    match std::str::from_utf8(head) {
        Ok(_) => return text_or_binary(head, Encoding::Utf8),
        Err(e) => {
            // A truncated multi-byte sequence at the very end is fine.
            if e.error_len().is_none() && std::str::from_utf8(&head[..e.valid_up_to()]).is_ok() {
                return text_or_binary(head, Encoding::Utf8);
            }
        }
    }
    text_or_binary(head, Encoding::Windows1252)
}

fn control_ratio(head: &[u8]) -> f64 {
    let ctrl = head
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B))
        .count();
    ctrl as f64 / head.len().max(1) as f64
}

fn text_or_binary(head: &[u8], enc: Encoding) -> Sniff {
    if control_ratio(head) > 0.05 {
        return Sniff::Binary {
            reason: "control characters",
        };
    }
    Sniff::Text {
        encoding: enc,
        bom_len: 0,
    }
}

fn looks_textual_utf16(head: &[u8], le: bool) -> bool {
    let mut ctrl = 0usize;
    let mut units = 0usize;
    for pair in head.as_chunks::<2>().0 {
        let u = if le {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        };
        units += 1;
        if u < 0x20 && !matches!(u, 0x09 | 0x0A | 0x0D | 0x0C) {
            ctrl += 1;
        }
    }
    units > 0 && (ctrl as f64 / units as f64) <= 0.05
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_boms_and_plain() {
        assert_eq!(
            sniff(&[0xEF, 0xBB, 0xBF, b'h', b'i']),
            Sniff::Text {
                encoding: Encoding::Utf8,
                bom_len: 3
            }
        );
        assert_eq!(
            sniff(&[0xFF, 0xFE, b'h', 0, b'i', 0]),
            Sniff::Text {
                encoding: Encoding::Utf16Le,
                bom_len: 2
            }
        );
        assert_eq!(
            sniff(b"hello world\nline two\n"),
            Sniff::Text {
                encoding: Encoding::Utf8,
                bom_len: 0
            }
        );
        assert_eq!(
            sniff(b""),
            Sniff::Text {
                encoding: Encoding::Utf8,
                bom_len: 0
            }
        );
    }

    #[test]
    fn utf16_without_bom() {
        let text: Vec<u8> = "hello world, this is a test line\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(
            sniff(&text),
            Sniff::Text {
                encoding: Encoding::Utf16Le,
                bom_len: 0
            }
        );
        let be: Vec<u8> = "hello world, this is a test line\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        assert_eq!(
            sniff(&be),
            Sniff::Text {
                encoding: Encoding::Utf16Be,
                bom_len: 0
            }
        );
    }

    #[test]
    fn binary_and_1252() {
        assert!(matches!(
            sniff(b"MZ\x90\x00\x03\x00\x00\x00"),
            Sniff::Binary { .. }
        ));
        assert!(matches!(
            sniff(&[0x01, 0x02, 0x03, 0x04, b'a', b'b']),
            Sniff::Binary { .. }
        ));
        assert_eq!(
            sniff(b"caf\xe9 au lait\n"),
            Sniff::Text {
                encoding: Encoding::Windows1252,
                bom_len: 0
            }
        );
        // Truncated UTF-8 sequence at the end of the sample is still UTF-8.
        let mut s = b"hello \xc3".to_vec();
        assert_eq!(
            sniff(&s),
            Sniff::Text {
                encoding: Encoding::Utf8,
                bom_len: 0
            }
        );
        s.push(0xA9);
        assert_eq!(
            sniff(&s),
            Sniff::Text {
                encoding: Encoding::Utf8,
                bom_len: 0
            }
        );
    }
}
