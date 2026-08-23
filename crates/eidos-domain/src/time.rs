//! Timestamp representation.
//!
//! All timestamps are stored as nanoseconds since the Unix epoch in a signed
//! 64-bit integer. Windows `FILETIME` values (100ns ticks since 1601-01-01)
//! convert losslessly into this representation for all practical dates.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UnixNanos(pub i64);

impl Serialize for UnixNanos {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::json::i64_string::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for UnixNanos {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::json::i64_string::deserialize(deserializer).map(Self)
    }
}

/// Difference between the Windows epoch (1601-01-01) and the Unix epoch
/// (1970-01-01) in 100ns ticks.
const FILETIME_UNIX_DIFF_TICKS: i64 = 116_444_736_000_000_000;

impl UnixNanos {
    pub const fn new(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Convert a Windows `FILETIME` expressed as a 64-bit tick count.
    pub const fn from_filetime_ticks(ticks: i64) -> Self {
        let unix_ticks = ticks - FILETIME_UNIX_DIFF_TICKS;
        Self(unix_ticks.saturating_mul(100))
    }

    /// Convert to Windows `FILETIME` ticks.
    pub const fn to_filetime_ticks(self) -> i64 {
        self.0 / 100 + FILETIME_UNIX_DIFF_TICKS
    }

    pub fn from_system_time(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Self(d.as_nanos().min(i64::MAX as u128) as i64),
            Err(e) => Self(-(e.duration().as_nanos().min(i64::MAX as u128) as i64)),
        }
    }

    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    pub const fn as_secs(self) -> i64 {
        self.0.div_euclid(1_000_000_000)
    }

    pub const fn as_millis(self) -> i64 {
        self.0.div_euclid(1_000_000)
    }

    /// Render as RFC 3339 UTC with millisecond precision.
    pub fn to_rfc3339(self) -> String {
        let secs = self.as_secs();
        let millis = (self.0.rem_euclid(1_000_000_000) / 1_000_000) as u32;
        let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
    }

    /// Render as RFC 3339 UTC with the full nanosecond field (9 fractional
    /// digits). Exports use this so no precision is lost or rounded away.
    pub fn to_rfc3339_nanos(self) -> String {
        let secs = self.as_secs();
        let nanos = self.0.rem_euclid(1_000_000_000);
        let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{nanos:09}Z")
    }

    /// Parse an RFC 3339 timestamp (UTC `Z` or numeric offset) or a bare date.
    pub fn parse(s: &str) -> Option<Self> {
        parse_rfc3339(s)
    }
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_rfc3339(s: &str) -> Option<UnixNanos> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(range)?;
        if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let y = num(0..4)?;
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let m = num(5..7)? as u32;
    let d = num(8..10)? as u32;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m, d) * 86_400;
    let mut nanos: i64 = 0;
    let mut idx = 10;
    if idx < b.len() && (b[idx] == b'T' || b[idx] == b't' || b[idx] == b' ') {
        idx += 1;
        let hh = num(idx..idx + 2)?;
        let mm = num(idx + 3..idx + 5)?;
        if b.get(idx + 2) != Some(&b':') || b.get(idx + 5) != Some(&b':') {
            return None;
        }
        let ss = num(idx + 6..idx + 8)?;
        if hh > 23 || mm > 59 || ss > 60 {
            return None;
        }
        secs += hh * 3600 + mm * 60 + ss;
        idx += 8;
        if idx < b.len() && b[idx] == b'.' {
            idx += 1;
            let start = idx;
            while idx < b.len() && b[idx].is_ascii_digit() {
                idx += 1;
            }
            let frac = &s[start..idx];
            if frac.is_empty() {
                return None;
            }
            let mut scale = 1_000_000_000i64;
            let mut val = 0i64;
            for c in frac.bytes().take(9) {
                scale /= 10;
                val = val * 10 + (c - b'0') as i64;
            }
            nanos = val * scale;
        }
        if idx < b.len() {
            match b[idx] {
                b'Z' | b'z' => idx += 1,
                b'+' | b'-' => {
                    let sign = if b[idx] == b'+' { 1 } else { -1 };
                    let oh = num(idx + 1..idx + 3)?;
                    let om = if b.get(idx + 3) == Some(&b':') {
                        num(idx + 4..idx + 6)?
                    } else {
                        num(idx + 3..idx + 5)?
                    };
                    secs -= sign * (oh * 3600 + om * 60);
                    idx = b.len();
                }
                _ => return None,
            }
        }
    }
    if idx != b.len() {
        return None;
    }
    Some(UnixNanos(
        secs.checked_mul(1_000_000_000)?.checked_add(nanos)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_roundtrip() {
        let ticks = 133_000_000_000_000_000i64;
        let t = UnixNanos::from_filetime_ticks(ticks);
        assert_eq!(t.to_filetime_ticks(), ticks);
    }

    #[test]
    fn epoch_renders() {
        assert_eq!(UnixNanos(0).to_rfc3339(), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            UnixNanos(1_700_000_000_000_000_000).to_rfc3339(),
            "2023-11-14T22:13:20.000Z"
        );
    }

    #[test]
    fn nanos_render_keeps_every_digit() {
        assert_eq!(
            UnixNanos(1_700_000_000_123_456_789).to_rfc3339_nanos(),
            "2023-11-14T22:13:20.123456789Z"
        );
        // Pre-epoch instants borrow from the second, as `to_rfc3339` does.
        assert_eq!(
            UnixNanos(-1).to_rfc3339_nanos(),
            "1969-12-31T23:59:59.999999999Z"
        );
    }

    #[test]
    fn parse_roundtrip() {
        let t = UnixNanos::parse("2026-08-22T01:02:03.250Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-08-22T01:02:03.250Z");
        let d = UnixNanos::parse("2026-08-22").unwrap();
        assert_eq!(d.to_rfc3339(), "2026-08-22T00:00:00.000Z");
        let off = UnixNanos::parse("2026-08-22T01:00:00-05:00").unwrap();
        assert_eq!(off.to_rfc3339(), "2026-08-22T06:00:00.000Z");
        assert!(UnixNanos::parse("not a date").is_none());
        assert!(UnixNanos::parse("2026-13-01").is_none());
    }

    #[test]
    fn json_is_a_decimal_string_but_legacy_numbers_still_parse() {
        let time = UnixNanos(1_700_000_000_123_456_789);
        assert_eq!(
            serde_json::to_string(&time).unwrap(),
            "\"1700000000123456789\""
        );
        assert_eq!(
            serde_json::from_str::<UnixNanos>("123").unwrap(),
            UnixNanos(123)
        );
    }
}
