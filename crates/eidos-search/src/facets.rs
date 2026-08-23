//! Range facet buckets: size and modification time.
//!
//! One table per facet is the single source of truth for three things that
//! have to agree exactly: the Tantivy range aggregation request, the label a
//! client shows, and the query clause a client appends when the bucket is
//! clicked. Nothing downstream re-derives a boundary, so a bucket and the
//! query it produces can never drift apart.
//!
//! Buckets are half-open — `from` inclusive, `to` exclusive — and the first
//! and last are open-ended. Time boundaries are aligned to UTC midnight so
//! that the clause is an absolute date the query language round-trips
//! exactly (`mtime:>=2026-08-16 mtime:<2026-08-23`) rather than a relative
//! window that means something different the moment it is re-run.

use eidos_domain::{FacetRange, ResultMode, UnixNanos};

/// Nanoseconds in a day.
pub const DAY_NS: i64 = 86_400_000_000_000;

/// Exclusive upper edges of the size buckets, in bytes.
const SIZE_EDGES: [i64; 6] = [4 << 10, 64 << 10, 1 << 20, 16 << 20, 256 << 20, 1 << 30];

/// Modification-time bucket edges, in whole UTC days before the start of the
/// current day.
const TIME_EDGE_DAYS: [i64; 5] = [0, 1, 7, 30, 365];

/// One bucket of a range facet, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeBucket {
    /// Inclusive lower bound: bytes, or Unix nanoseconds for time buckets.
    pub from: Option<i64>,
    /// Exclusive upper bound, same units as `from`.
    pub to: Option<i64>,
    /// Human label; always spells the exact boundaries.
    pub label: String,
    /// Clauses that select or exclude the bucket. `None` when no clause
    /// reproduces the bucket in this result mode (see [`size_buckets`]).
    pub range: Option<FacetRange>,
}

/// Size buckets, smallest first.
///
/// The aggregation runs over the `subtree_logical` fast field, which holds a
/// file's own size and a directory's subtree size. In `files` mode that is
/// the file size (`size:`) and in `directories` mode the subtree size
/// (`subtree:`); in `both` mode the buckets mix the two and no single clause
/// reproduces them, so they carry no clause and stay display-only.
pub fn size_buckets(mode: ResultMode) -> Vec<RangeBucket> {
    let key = match mode {
        ResultMode::Files => Some("size"),
        ResultMode::Directories => Some("subtree"),
        ResultMode::Both => None,
    };
    let mut out = Vec::with_capacity(SIZE_EDGES.len() + 1);
    let mut from = None;
    for to in SIZE_EDGES.iter().copied().map(Some).chain([None]) {
        out.push(RangeBucket {
            from,
            to,
            label: size_label(from, to),
            range: key.map(|k| clauses(k, from, to, size_token)),
        });
        from = to;
    }
    out
}

/// Modification-time buckets, newest first.
///
/// Boundaries are UTC midnights relative to the start of the day containing
/// `now`. The aggregation runs over the `newest_modified` fast field, which
/// holds a file's own modification time and a directory's newest descendant
/// modification time — `mtime:` and `subtree_mtime:` respectively. As with
/// [`size_buckets`], `both` mode mixes the two and carries no clause.
pub fn time_buckets(now: UnixNanos, mode: ResultMode) -> Vec<RangeBucket> {
    let key = match mode {
        ResultMode::Files => Some("mtime"),
        ResultMode::Directories => Some("subtree_mtime"),
        ResultMode::Both => None,
    };
    let today = now.0 - now.0.rem_euclid(DAY_NS);
    let mut out = Vec::with_capacity(TIME_EDGE_DAYS.len() + 1);
    let mut from = None;
    let edges = TIME_EDGE_DAYS
        .iter()
        .rev()
        .map(|d| Some(today - d * DAY_NS))
        .chain([None]);
    for to in edges {
        out.push(RangeBucket {
            from,
            to,
            label: time_label(from, to),
            range: key.map(|k| clauses(k, from, to, date_token)),
        });
        from = to;
    }
    out.reverse();
    out
}

/// The include and exclude clauses for one half-open bucket.
fn clauses(key: &str, from: Option<i64>, to: Option<i64>, token: fn(i64) -> String) -> FacetRange {
    let clause = match (from, to) {
        (None, Some(t)) => format!("{key}:<{}", token(t)),
        (Some(f), None) => format!("{key}:>={}", token(f)),
        (Some(f), Some(t)) => format!("{key}:>={} {key}:<{}", token(f), token(t)),
        (None, None) => "*".to_string(),
    };
    // A bounded bucket is two clauses, so its negation has to be grouped:
    // `-size:>=1M size:<16M` would exclude only the lower bound.
    let exclude = if clause.contains(' ') {
        format!("-({clause})")
    } else {
        format!("-{clause}")
    };
    FacetRange {
        from,
        to,
        clause,
        exclude,
    }
}

/// A byte count as a query token, using a binary suffix only when it is exact.
fn size_token(bytes: i64) -> String {
    for (shift, suffix) in [(30, 'G'), (20, 'M'), (10, 'k')] {
        let unit = 1i64 << shift;
        if bytes >= unit && bytes % unit == 0 {
            return format!("{}{suffix}", bytes / unit);
        }
    }
    bytes.to_string()
}

/// A UTC-midnight timestamp as a `YYYY-MM-DD` query token.
fn date_token(nanos: i64) -> String {
    UnixNanos(nanos).to_rfc3339()[..10].to_string()
}

fn size_label(from: Option<i64>, to: Option<i64>) -> String {
    match (from, to) {
        (None, Some(t)) => format!("< {}", human_bytes(t)),
        (Some(f), None) => format!("≥ {}", human_bytes(f)),
        (Some(f), Some(t)) => format!("≥ {}, < {}", human_bytes(f), human_bytes(t)),
        (None, None) => "any size".to_string(),
    }
}

/// Labels name both boundaries. Closed buckets show the inclusive last day
/// they cover, so no reader has to reason about the exclusive end.
fn time_label(from: Option<i64>, to: Option<i64>) -> String {
    match (from, to) {
        (None, Some(t)) => format!("older · < {} UTC", date_token(t)),
        (Some(f), None) => format!("today · ≥ {} UTC", date_token(f)),
        (Some(f), Some(t)) if t - f <= DAY_NS => format!("{} UTC", date_token(f)),
        (Some(f), Some(t)) => format!("{} … {} UTC", date_token(f), date_token(t - DAY_NS)),
        (None, None) => "any time".to_string(),
    }
}

fn human_bytes(bytes: i64) -> String {
    for (shift, unit) in [(30, "GiB"), (20, "MiB"), (10, "KiB")] {
        let scale = 1i64 << shift;
        if bytes >= scale && bytes % scale == 0 {
            return format!("{} {unit}", bytes / scale);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidos_domain::{Query, SizeField, TimeField};
    use eidos_query::parser::parse_at;
    use eidos_query::render;

    /// 2026-08-23T13:45:12Z — deliberately not midnight.
    const NOW: UnixNanos = UnixNanos::new(1_787_492_712_000_000_000);

    fn bounds(clause: &str) -> (Option<i64>, Option<i64>) {
        let parsed = parse_at(clause, NOW).expect("clause parses");
        let mut lo = None;
        let mut hi = None;
        let mut visit = |q: &Query| match q {
            Query::Size { min, max, .. } | Query::SubtreeSize { min, max, .. } => {
                lo = min.map(|v| v as i64).or(lo);
                hi = max.map(|v| v as i64 + 1).or(hi);
            }
            Query::Time { after, before, .. } => {
                lo = after.map(|t| t.0).or(lo);
                hi = before.map(|t| t.0).or(hi);
            }
            _ => panic!("unexpected clause {q:?}"),
        };
        match &parsed.query {
            Query::And { clauses } => clauses.iter().for_each(&mut visit),
            other => visit(other),
        }
        (lo, hi)
    }

    #[test]
    fn size_bucket_clauses_parse_back_to_the_bucket() {
        let buckets = size_buckets(ResultMode::Files);
        assert_eq!(buckets.len(), 7);
        for b in &buckets {
            let range = b.range.as_ref().expect("files mode carries clauses");
            assert_eq!(bounds(&range.clause), (b.from, b.to), "{}", range.clause);
        }
        assert_eq!(buckets[0].range.as_ref().unwrap().clause, "size:<4k");
        assert_eq!(buckets[0].label, "< 4 KiB");
        assert_eq!(
            buckets[2].range.as_ref().unwrap().clause,
            "size:>=64k size:<1M"
        );
        assert_eq!(buckets[2].label, "≥ 64 KiB, < 1 MiB");
        assert_eq!(buckets[6].range.as_ref().unwrap().clause, "size:>=1G");
        assert_eq!(buckets[6].label, "≥ 1 GiB");
    }

    #[test]
    fn size_bucket_exclusions_negate_the_whole_bucket() {
        let buckets = size_buckets(ResultMode::Files);
        let first = buckets[0].range.as_ref().unwrap();
        assert_eq!(first.exclude, "-size:<4k");
        let middle = buckets[3].range.as_ref().unwrap();
        assert_eq!(middle.exclude, "-(size:>=1M size:<16M)");
        // The grouped form negates both bounds, not just the first.
        let parsed = parse_at(&middle.exclude, NOW).unwrap().query;
        let Query::Not { clause } = &parsed else {
            panic!("expected a negation, got {parsed:?}")
        };
        let Query::And { clauses } = clause.as_ref() else {
            panic!("expected two bounds, got {clause:?}")
        };
        assert_eq!(clauses.len(), 2);
        // The renderer spells the upper bound inclusively, but the grouped
        // negation and both bounds survive.
        assert_eq!(render(&parsed), "-(size:>=1M size:<=16777215)");
        assert_eq!(parse_at(&render(&parsed), NOW).unwrap().query, parsed);
    }

    #[test]
    fn directory_mode_buckets_use_the_subtree_fields() {
        let sizes = size_buckets(ResultMode::Directories);
        assert_eq!(
            sizes[4].range.as_ref().unwrap().clause,
            "subtree:>=16M subtree:<256M"
        );
        assert!(matches!(
            parse_at(&sizes[4].range.as_ref().unwrap().clause, NOW)
                .unwrap()
                .query,
            Query::And { .. }
        ));
        let times = time_buckets(NOW, ResultMode::Directories);
        let clause = &times[0].range.as_ref().unwrap().clause;
        assert_eq!(clause, "subtree_mtime:>=2026-08-23");
        assert!(matches!(
            parse_at(clause, NOW).unwrap().query,
            Query::Time {
                field: TimeField::SubtreeModified,
                ..
            }
        ));
        // `subtree:` is the logical, not the allocated, subtree size.
        assert!(matches!(
            parse_at("subtree:>=16M", NOW).unwrap().query,
            Query::SubtreeSize {
                field: SizeField::Logical,
                ..
            }
        ));
    }

    #[test]
    fn time_buckets_are_utc_day_aligned_and_contiguous() {
        let buckets = time_buckets(NOW, ResultMode::Files);
        assert_eq!(buckets.len(), 6);
        // Newest first, contiguous, open at both ends.
        assert_eq!(buckets[0].to, None);
        assert_eq!(buckets[5].from, None);
        for pair in buckets.windows(2) {
            assert_eq!(pair[0].from, pair[1].to);
        }
        let day = UnixNanos::new(1_787_443_200_000_000_000); // 2026-08-23T00:00Z
        assert_eq!(buckets[0].from, Some(day.0));
        for b in &buckets {
            assert!(b.from.is_none_or(|v| v.rem_euclid(DAY_NS) == 0));
            let range = b.range.as_ref().expect("files mode carries clauses");
            assert_eq!(bounds(&range.clause), (b.from, b.to), "{}", range.clause);
        }
        assert_eq!(
            buckets[0].range.as_ref().unwrap().clause,
            "mtime:>=2026-08-23"
        );
        assert_eq!(buckets[0].label, "today · ≥ 2026-08-23 UTC");
        assert_eq!(buckets[1].label, "2026-08-22 UTC");
        assert_eq!(
            buckets[2].range.as_ref().unwrap().clause,
            "mtime:>=2026-08-16 mtime:<2026-08-22"
        );
        assert_eq!(buckets[2].label, "2026-08-16 … 2026-08-21 UTC");
        assert_eq!(
            buckets[5].range.as_ref().unwrap().clause,
            "mtime:<2025-08-23"
        );
        assert_eq!(buckets[5].label, "older · < 2025-08-23 UTC");
    }

    #[test]
    fn both_mode_buckets_are_display_only() {
        assert!(size_buckets(ResultMode::Both)
            .iter()
            .all(|b| b.range.is_none()));
        assert!(time_buckets(NOW, ResultMode::Both)
            .iter()
            .all(|b| b.range.is_none()));
        // Labels are still exact.
        assert_eq!(size_buckets(ResultMode::Both)[1].label, "≥ 4 KiB, < 64 KiB");
    }

    #[test]
    fn bucket_clauses_survive_a_render_round_trip() {
        for mode in [ResultMode::Files, ResultMode::Directories] {
            let all = size_buckets(mode)
                .into_iter()
                .chain(time_buckets(NOW, mode))
                .filter_map(|b| b.range);
            for range in all {
                for text in [&range.clause, &range.exclude] {
                    let q = parse_at(text, NOW).unwrap().query;
                    let again = parse_at(&render(&q), NOW).unwrap().query;
                    assert_eq!(q, again, "{text} -> {}", render(&q));
                }
            }
        }
    }
}
