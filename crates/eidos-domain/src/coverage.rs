//! Typed coverage envelope: how completely this answer consulted the corpus.
//!
//! Every search response carries a [`CoverageEnvelope`] summarizing, in typed
//! form, everything that degraded the answer — per source and for the
//! response as a whole. It is derived from the same facts as
//! [`SourceCompleteness`](crate::result::SourceCompleteness) but is the
//! contract clients should branch on: an offline source is a first-class,
//! severity-graded state with a watermark ("authoritative as of"), not an
//! error, and absence of degradation is stated (`full`), never inferred.

use crate::ids::SourceId;
use crate::result::{Freshness, SourceCompleteness};
use crate::state::SourceState;
use crate::time::UnixNanos;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, TS)]
pub struct CoverageEnvelope {
    /// True only when nothing degraded this answer: no response-level reasons
    /// and no per-source reasons. Clients must branch on this, never on the
    /// absence of hits or banners.
    pub full: bool,
    /// Degradations that apply to the whole result set (budget truncation,
    /// index lag, deadline hits).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<CoverageReason>,
    /// One entry per source in scope, always present, even when clean.
    pub sources: Vec<SourceCoverage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SourceCoverage {
    pub source_id: SourceId,
    pub name: String,
    /// The instant up to which this source's results are authoritative: now
    /// for a healthy live-feed source, otherwise the last completed scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub watermark: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<CoverageReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CoverageReason {
    pub kind: CoverageKind,
    pub severity: CoverageSeverity,
    /// Human-readable statement of what is degraded and by how much.
    pub detail: String,
    /// What (if anything) the operator can do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub remediation: Option<String>,
}

/// Typed degradation classes. Fleet-era variants (`GenerationReset`,
/// `ContentNotReplicated`, `Timeout`) are part of the wire contract now so the
/// envelope shape survives multi-host unchanged; single-host emits the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CoverageKind {
    /// Source unreachable; results preserved as of the watermark.
    Offline,
    /// Freshness guarantee expired (periodic source past its interval).
    Stale,
    /// Change feed overflowed or checkpoint invalid; reconciliation pending.
    DegradedFeed,
    /// First enumeration still running; listings incomplete until publish.
    Enumerating,
    /// Reconciliation scan running; last generation plus new observations.
    Reconciling,
    /// No published generation at all; the source cannot contribute.
    NotScanned,
    /// The search index is missing or behind the published catalog generation.
    NotIndexed,
    /// Catalog changes not yet reflected in the search index (follower lag).
    IndexLag,
    /// Content jobs outstanding for a query that consults content.
    ContentPending,
    /// Content extraction failed for some files in scope.
    ContentFailed,
    /// Directories that could not be listed; previous contents preserved.
    ListingErrors,
    /// The walk stopped at a budget; totals are a lower bound.
    Truncated,
    /// A deadline elapsed before the answer was complete.
    Timeout,
    /// A source epoch changed and history was discarded (fleet).
    GenerationReset,
    /// Content exists at the origin but is not replicated here (fleet).
    ContentNotReplicated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CoverageSeverity {
    /// The answer is trustworthy; something is worth knowing.
    Info,
    /// Parts of the corpus may be missing or out of date.
    Warning,
    /// A source in scope contributes nothing.
    Error,
}

impl std::fmt::Display for CoverageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Offline => "offline",
            Self::Stale => "stale",
            Self::DegradedFeed => "degraded_feed",
            Self::Enumerating => "enumerating",
            Self::Reconciling => "reconciling",
            Self::NotScanned => "not_scanned",
            Self::NotIndexed => "not_indexed",
            Self::IndexLag => "index_lag",
            Self::ContentPending => "content_pending",
            Self::ContentFailed => "content_failed",
            Self::ListingErrors => "listing_errors",
            Self::Truncated => "truncated",
            Self::Timeout => "timeout",
            Self::GenerationReset => "generation_reset",
            Self::ContentNotReplicated => "content_not_replicated",
        };
        f.write_str(s)
    }
}

impl std::fmt::Display for CoverageSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

/// Response-level facts the executor knows that per-source completeness rows
/// do not carry.
#[derive(Debug, Clone, Default)]
pub struct ResponseSignals {
    /// Outbox rows not yet applied by the index follower.
    pub index_lag: u64,
    /// Whether this query consults content (content clauses present).
    pub content_query: bool,
    /// The content index is rebuilding/incomplete (reason), if so.
    pub content_index_rebuilding: Option<String>,
    /// The reported total is a lower bound (budget-stopped walk).
    pub total_is_bound: bool,
}

impl CoverageEnvelope {
    /// Derive the envelope from per-source completeness plus response-level
    /// signals. Pure so the CLI, service, and tests share one meaning.
    pub fn derive(
        completeness: &[SourceCompleteness],
        signals: &ResponseSignals,
        now: UnixNanos,
    ) -> Self {
        let mut degraded = Vec::new();
        if signals.total_is_bound {
            degraded.push(CoverageReason {
                kind: CoverageKind::Truncated,
                severity: CoverageSeverity::Info,
                detail: "the result walk stopped at its budget; the total is a lower bound".into(),
                remediation: Some("narrow the query to make the count exact".into()),
            });
        }
        if signals.index_lag > 0 {
            degraded.push(CoverageReason {
                kind: CoverageKind::IndexLag,
                severity: CoverageSeverity::Info,
                detail: format!(
                    "{} catalog change(s) not yet reflected in the search index",
                    signals.index_lag
                ),
                remediation: Some(
                    "retry in a moment; the follower applies changes continuously".into(),
                ),
            });
        }
        if signals.content_query {
            if let Some(reason) = &signals.content_index_rebuilding {
                degraded.push(CoverageReason {
                    kind: CoverageKind::NotIndexed,
                    severity: CoverageSeverity::Warning,
                    detail: reason.clone(),
                    remediation: Some("content results fill in as the rebuild progresses".into()),
                });
            }
        }
        let sources = completeness
            .iter()
            .map(|c| source_coverage(c, signals, now))
            .collect::<Vec<_>>();
        let full = degraded.is_empty()
            && sources
                .iter()
                .all(|s: &SourceCoverage| s.degraded.is_empty());
        Self {
            full,
            degraded,
            sources,
        }
    }
}

fn source_coverage(
    c: &SourceCompleteness,
    signals: &ResponseSignals,
    now: UnixNanos,
) -> SourceCoverage {
    let mut degraded = Vec::new();
    let last = c
        .last_scan_completed
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "never".into());
    match c.state {
        SourceState::New => degraded.push(CoverageReason {
            kind: CoverageKind::NotScanned,
            severity: CoverageSeverity::Error,
            detail: format!("{} has never been scanned; it contributes nothing", c.name),
            remediation: Some(format!("run `eidos source scan` for {}", c.name)),
        }),
        SourceState::Enumerating => degraded.push(CoverageReason {
            kind: CoverageKind::Enumerating,
            severity: CoverageSeverity::Warning,
            detail: format!(
                "{} is being enumerated for the first time; listings are incomplete until the scan publishes",
                c.name
            ),
            remediation: None,
        }),
        SourceState::Reconciling => degraded.push(CoverageReason {
            kind: CoverageKind::Reconciling,
            severity: CoverageSeverity::Info,
            detail: format!(
                "{} is being rescanned; showing the last published generation plus new observations",
                c.name
            ),
            remediation: None,
        }),
        SourceState::Offline => degraded.push(CoverageReason {
            kind: CoverageKind::Offline,
            severity: CoverageSeverity::Info,
            detail: format!(
                "{} is unreachable; results are preserved as of {last}",
                c.name
            ),
            remediation: Some(
                "reconnect the source; results heal automatically when it returns".into(),
            ),
        }),
        SourceState::Stale => degraded.push(CoverageReason {
            kind: CoverageKind::Stale,
            severity: CoverageSeverity::Info,
            detail: format!(
                "{} is past its freshness interval; results are as of {last}",
                c.name
            ),
            remediation: Some(format!("run `eidos source scan` for {} to refresh", c.name)),
        }),
        SourceState::Degraded => degraded.push(CoverageReason {
            kind: CoverageKind::DegradedFeed,
            severity: CoverageSeverity::Warning,
            detail: format!(
                "{}'s change feed overflowed or its checkpoint is invalid; results are preserved as of {last}",
                c.name
            ),
            remediation: Some("a reconciliation scan restores live freshness automatically".into()),
        }),
        SourceState::MetadataComplete
        | SourceState::ContentPending
        | SourceState::Complete
        | SourceState::Retired => {}
    }
    // Index-behind-catalog states discovered by the executor land in `note`
    // with `metadata_complete` cleared while the source state itself claims
    // completeness; surface them as NotIndexed rather than silently degrading.
    let state_claims_complete = matches!(
        c.state,
        SourceState::MetadataComplete
            | SourceState::ContentPending
            | SourceState::Complete
            | SourceState::Reconciling
    );
    // A remote source in Reconciling is truthfully reporting that its fleet
    // snapshot or aggregate publication is incomplete. That is not a search
    // index failure; the catalog has deliberately withheld publication.
    let replica_is_reconciling = c.state == SourceState::Reconciling && c.content_not_replicated;
    if !c.metadata_complete && state_claims_complete && !replica_is_reconciling {
        degraded.push(CoverageReason {
            kind: CoverageKind::NotIndexed,
            severity: CoverageSeverity::Warning,
            detail: c.note.clone().unwrap_or_else(|| {
                format!("{}'s search index is behind its published catalog", c.name)
            }),
            remediation: Some("the index rebuilds automatically; retry shortly".into()),
        });
    }
    if c.listing_errors > 0 {
        degraded.push(CoverageReason {
            kind: CoverageKind::ListingErrors,
            severity: CoverageSeverity::Warning,
            detail: format!(
                "{} director{} could not be listed; previous contents were preserved and their totals are marked incomplete",
                c.listing_errors,
                if c.listing_errors == 1 { "y" } else { "ies" }
            ),
            remediation: Some("see the source's errors view for the affected paths".into()),
        });
    }
    if signals.content_query {
        if c.content_not_replicated {
            degraded.push(CoverageReason {
                kind: CoverageKind::ContentNotReplicated,
                severity: CoverageSeverity::Info,
                detail: format!(
                    "{} replicates metadata only and cannot contribute content matches",
                    c.name
                ),
                remediation: None,
            });
        }
        if c.content_pending > 0 {
            degraded.push(CoverageReason {
                kind: CoverageKind::ContentPending,
                severity: CoverageSeverity::Info,
                detail: format!(
                    "{} file(s) in {} still await content indexing",
                    c.content_pending, c.name
                ),
                remediation: None,
            });
        }
        if c.content_failed > 0 {
            degraded.push(CoverageReason {
                kind: CoverageKind::ContentFailed,
                severity: CoverageSeverity::Warning,
                detail: format!(
                    "content extraction failed for {} file(s) in {}",
                    c.content_failed, c.name
                ),
                remediation: Some("see the errors view; failed files can be requeued".into()),
            });
        }
    }
    // Reconciling is deliberately excluded: a rescan is running because the
    // guarantee lapsed, so results are only authoritative as of the last
    // completed scan even while the change feed is live.
    let healthy_live = c.freshness == Freshness::Live
        && matches!(
            c.state,
            SourceState::MetadataComplete | SourceState::ContentPending | SourceState::Complete
        );
    SourceCoverage {
        source_id: c.source_id,
        name: c.name.clone(),
        watermark: if healthy_live {
            Some(now)
        } else {
            c.last_scan_completed
        },
        degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(name: &str) -> SourceCompleteness {
        SourceCompleteness {
            source_id: SourceId(1),
            name: name.into(),
            state: SourceState::Complete,
            metadata_complete: true,
            content_complete: true,
            content_not_replicated: false,
            content_pending: 0,
            content_failed: 0,
            listing_errors: 0,
            last_scan_completed: Some(UnixNanos::new(1_000)),
            checkpoint_age_ms: Some(10),
            freshness: Freshness::Live,
            note: None,
        }
    }

    #[test]
    fn healthy_is_full_with_live_watermark() {
        let now = UnixNanos::new(5_000);
        let env = CoverageEnvelope::derive(&[complete("g")], &ResponseSignals::default(), now);
        assert!(env.full);
        assert!(env.degraded.is_empty());
        assert_eq!(env.sources.len(), 1);
        assert!(env.sources[0].degraded.is_empty());
        // Live feed: authoritative now, not merely as of the last scan.
        assert_eq!(env.sources[0].watermark, Some(now));
    }

    #[test]
    fn offline_is_info_grade_with_scan_watermark() {
        let mut c = complete("smb");
        c.state = SourceState::Offline;
        c.freshness = Freshness::Unknown;
        let env = CoverageEnvelope::derive(&[c], &ResponseSignals::default(), UnixNanos::new(9));
        assert!(!env.full);
        let r = &env.sources[0].degraded[0];
        assert_eq!(r.kind, CoverageKind::Offline);
        assert_eq!(r.severity, CoverageSeverity::Info);
        assert!(r.remediation.is_some());
        assert_eq!(env.sources[0].watermark, Some(UnixNanos::new(1_000)));
    }

    #[test]
    fn content_state_only_degrades_content_queries() {
        let mut c = complete("g");
        c.content_pending = 7;
        c.content_failed = 2;
        c.content_complete = false;
        let metadata_only =
            CoverageEnvelope::derive(&[c.clone()], &ResponseSignals::default(), UnixNanos::new(0));
        assert!(metadata_only.full, "metadata answers are complete");
        let content = CoverageEnvelope::derive(
            &[c],
            &ResponseSignals {
                content_query: true,
                ..ResponseSignals::default()
            },
            UnixNanos::new(0),
        );
        assert!(!content.full);
        let kinds: Vec<_> = content.sources[0].degraded.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![CoverageKind::ContentPending, CoverageKind::ContentFailed]
        );
    }

    #[test]
    fn metadata_only_replica_degrades_only_content_queries() {
        let mut c = complete("laptop/fleet");
        c.content_complete = false;
        c.content_not_replicated = true;
        let metadata =
            CoverageEnvelope::derive(&[c.clone()], &ResponseSignals::default(), UnixNanos::new(0));
        assert!(metadata.full, "metadata answers remain complete");

        let content = CoverageEnvelope::derive(
            &[c],
            &ResponseSignals {
                content_query: true,
                ..ResponseSignals::default()
            },
            UnixNanos::new(0),
        );
        assert!(!content.full);
        assert_eq!(content.sources[0].degraded.len(), 1);
        assert_eq!(
            content.sources[0].degraded[0].kind,
            CoverageKind::ContentNotReplicated
        );
    }

    #[test]
    fn response_level_signals_surface() {
        let env = CoverageEnvelope::derive(
            &[complete("g")],
            &ResponseSignals {
                index_lag: 3,
                total_is_bound: true,
                ..ResponseSignals::default()
            },
            UnixNanos::new(0),
        );
        assert!(!env.full);
        let kinds: Vec<_> = env.degraded.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![CoverageKind::Truncated, CoverageKind::IndexLag]);
    }

    #[test]
    fn never_scanned_is_an_error() {
        let mut c = complete("new");
        c.state = SourceState::New;
        c.metadata_complete = false;
        c.last_scan_completed = None;
        let env = CoverageEnvelope::derive(&[c], &ResponseSignals::default(), UnixNanos::new(0));
        let r = &env.sources[0].degraded[0];
        assert_eq!(r.kind, CoverageKind::NotScanned);
        assert_eq!(r.severity, CoverageSeverity::Error);
        assert_eq!(env.sources[0].degraded.len(), 1, "no duplicate NotIndexed");
        assert_eq!(env.sources[0].watermark, None);
    }

    #[test]
    fn reconciling_keeps_the_last_scan_watermark() {
        let mut c = complete("g");
        c.state = SourceState::Reconciling;
        let env =
            CoverageEnvelope::derive(&[c], &ResponseSignals::default(), UnixNanos::new(5_000));
        let r = &env.sources[0].degraded[0];
        assert_eq!(r.kind, CoverageKind::Reconciling);
        assert_eq!(r.severity, CoverageSeverity::Info);
        assert_eq!(
            env.sources[0].watermark,
            Some(UnixNanos::new(1_000)),
            "a live feed does not make mid-reconciliation results authoritative now"
        );
    }

    #[test]
    fn a_reconciling_replica_is_not_misreported_as_an_index_failure() {
        let mut c = complete("remote");
        c.state = SourceState::Reconciling;
        c.metadata_complete = false;
        c.content_not_replicated = true;
        c.note = Some("fleet snapshot is still arriving".into());
        let env =
            CoverageEnvelope::derive(&[c], &ResponseSignals::default(), UnixNanos::new(5_000));
        let reasons = &env.sources[0].degraded;
        assert!(reasons.iter().any(|r| r.kind == CoverageKind::Reconciling));
        assert!(!reasons.iter().any(|r| r.kind == CoverageKind::NotIndexed));
    }
}
