//! Interaction events: what a search presented, and what happened next.
//!
//! This is data collection only. Nothing here feeds ranking, and no reader
//! outside the analysis tooling looks at the table; recording an event must
//! never change what a search returns.
//!
//! Two rules shape the schema:
//!
//! 1. **No query text.** A row keeps [`query_hash`] — a digest of the
//!    normalized query — and a coarse [`QueryShape`]. That is enough to group
//!    the events of one query and to compare shapes against each other, and
//!    it keeps the catalog from becoming a log of what anyone searched for.
//!    The digest is a grouping key, not an anonymity guarantee: a short query
//!    can be confirmed by anyone who guesses it and hashes the guess.
//! 2. **Bounded growth.** [`Catalog::prune_interactions`] enforces both an age
//!    limit and a hard row cap, and the insert path calls it periodically so
//!    a long-running service bounds the table without an operator.
//!
//! Timestamps are assigned where the row is written, never by the client: a
//! skewed or hostile clock must not be able to park rows in the future where
//! the age limit can never reach them.

use crate::{Catalog, Result};
use eidos_domain::{ObjectId, Query, SourceId, TextField, TextMode, UnixNanos};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Hard cap on stored events. The oldest rows beyond it are deleted.
pub const MAX_INTERACTION_ROWS: u64 = 1_000_000;
/// Events older than this are deleted.
pub const INTERACTION_MAX_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// Longest accepted opaque session id.
pub const MAX_SESSION_ID_LEN: usize = 64;
/// Prune inside the insert transaction once every this many batches, counting
/// from the first. Recording is on the interactive path, so the bound is
/// maintained in small periodic steps rather than on every batch; the service
/// also prunes once at startup, which covers a process that never gets there.
const PRUNE_EVERY_BATCHES: u64 = 32;

/// What someone did with a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionAction {
    /// The hit was rendered in a page of results.
    Presented,
    OpenedPreview,
    OpenedFile,
    CopiedPath,
    Exported,
}

impl InteractionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            InteractionAction::Presented => "presented",
            InteractionAction::OpenedPreview => "opened_preview",
            InteractionAction::OpenedFile => "opened_file",
            InteractionAction::CopiedPath => "copied_path",
            InteractionAction::Exported => "exported",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "presented" => InteractionAction::Presented,
            "opened_preview" => InteractionAction::OpenedPreview,
            "opened_file" => InteractionAction::OpenedFile,
            "copied_path" => InteractionAction::CopiedPath,
            "exported" => InteractionAction::Exported,
            _ => return None,
        })
    }
}

/// Coarse label for the kind of work a query asked for. Ordered from cheapest
/// to most specific so a mixed query takes the label of its strongest clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryShape {
    /// Only structural predicates: extension, size, time, source, kind…
    Metadata,
    /// A name or path predicate, with no content clause.
    Name,
    /// Scored, phrase, or exact content text.
    ContentRanked,
    /// Content substring or regular expression (candidate + verify).
    ContentRegex,
}

impl QueryShape {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryShape::Metadata => "metadata",
            QueryShape::Name => "name",
            QueryShape::ContentRanked => "content_ranked",
            QueryShape::ContentRegex => "content_regex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "metadata" => QueryShape::Metadata,
            "name" => QueryShape::Name,
            "content_ranked" => QueryShape::ContentRanked,
            "content_regex" => QueryShape::ContentRegex,
            _ => return None,
        })
    }

    /// Label a parsed query by its strongest clause.
    pub fn classify(query: &Query) -> Self {
        let mut shape = QueryShape::Metadata;
        query.visit(&mut |clause| {
            let found = match clause {
                Query::Text {
                    field: TextField::Content,
                    mode,
                    ..
                } => match mode {
                    TextMode::Substring | TextMode::Regex => QueryShape::ContentRegex,
                    _ => QueryShape::ContentRanked,
                },
                Query::Text { .. } | Query::Path { .. } => QueryShape::Name,
                _ => QueryShape::Metadata,
            };
            shape = shape.max(found);
        });
        shape
    }
}

/// Stable digest of a query, for grouping events without keeping the text.
///
/// The input is normalized first (trimmed, internal whitespace collapsed) so
/// incidental spacing does not split one query into several groups. Callers
/// that can parse the query should hash its canonical rendering instead, so
/// that clause order does not split groups either.
pub fn query_hash(text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"eidos-interaction-query-v1\0");
    let mut first = true;
    for word in text.split_whitespace() {
        if !first {
            hasher.update(b" ");
        }
        first = false;
        hasher.update(word.as_bytes());
    }
    // 128 bits: far more than enough to keep distinct queries distinct in a
    // table capped at a million rows, and half the storage of the full digest.
    hasher.finalize().to_hex()[..32].to_string()
}

/// One recorded interaction. `ts` is unix nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionEvent {
    pub ts: UnixNanos,
    pub query_hash: String,
    pub query_shape: QueryShape,
    pub object_id: Option<ObjectId>,
    pub source_id: Option<SourceId>,
    /// 0-based position of the hit in the result set it was presented in.
    pub presented_rank: Option<u32>,
    pub action: InteractionAction,
    /// Opaque per-tab (web) or per-invocation (CLI) id. Never a user id.
    pub session_id: String,
}

/// Bounds on how much history the table keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionRetention {
    pub max_age: Duration,
    pub max_rows: u64,
}

impl Default for InteractionRetention {
    fn default() -> Self {
        Self {
            max_age: INTERACTION_MAX_AGE,
            max_rows: MAX_INTERACTION_ROWS,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct InteractionStats {
    pub events: u64,
    pub sessions: u64,
    pub oldest_ts: Option<UnixNanos>,
    pub newest_ts: Option<UnixNanos>,
}

const INSERT_SQL: &str = "INSERT INTO interaction_events
        (ts, query_hash, query_shape, object_id, source_id, presented_rank, action, session_id)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

impl Catalog {
    /// Append a batch of events, and periodically enforce retention in the
    /// same transaction.
    ///
    /// One transaction through the shared writer gate per batch: the events of
    /// a batch are inserted in order, so `id` order is arrival order.
    pub fn record_interactions(&self, events: &[InteractionEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        let batch = self.interaction_batches.fetch_add(1, Ordering::Relaxed);
        let retention = InteractionRetention::default();
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            {
                let mut stmt = tx.prepare_cached(INSERT_SQL)?;
                for e in events {
                    stmt.execute(params![
                        e.ts.0,
                        e.query_hash,
                        e.query_shape.as_str(),
                        e.object_id.map(|o| o.0),
                        e.source_id.map(|s| s.0),
                        e.presented_rank,
                        e.action.as_str(),
                        e.session_id,
                    ])?;
                }
            }
            if (batch + 1) % PRUNE_EVERY_BATCHES == 0 {
                prune_within(&tx, retention)?;
            }
            tx.commit()?;
            Ok(events.len() as u64)
        })
    }

    /// Delete events past the age limit, then the oldest rows past the cap.
    /// Returns how many rows went away.
    pub fn prune_interactions(&self, retention: InteractionRetention) -> Result<u64> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let pruned = prune_within(&tx, retention)?;
            tx.commit()?;
            Ok(pruned)
        })
    }

    /// The newest `limit` events, oldest first. For analysis tooling and for
    /// tests; nothing on the search path reads interaction events.
    pub fn recent_interactions(&self, limit: u32) -> Result<Vec<InteractionEvent>> {
        /// A row as stored: the two labels are text and may not parse.
        struct Row {
            ts: i64,
            query_hash: String,
            query_shape: String,
            object_id: Option<i64>,
            source_id: Option<i64>,
            presented_rank: Option<u32>,
            action: String,
            session_id: String,
        }
        let raw: Vec<Row> = self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT ts, query_hash, query_shape, object_id, source_id, presented_rank,
                            action, session_id
                     FROM interaction_events ORDER BY id DESC LIMIT ?1",
                )?
                .query_map(params![limit], |r| {
                    Ok(Row {
                        ts: r.get(0)?,
                        query_hash: r.get(1)?,
                        query_shape: r.get(2)?,
                        object_id: r.get(3)?,
                        source_id: r.get(4)?,
                        presented_rank: r.get(5)?,
                        action: r.get(6)?,
                        session_id: r.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        raw.into_iter()
            .rev()
            .map(|row| {
                let invalid = |what: &str, value: &str| {
                    crate::CatalogError::InvalidState(format!(
                        "interaction event has an unknown {what} '{value}'"
                    ))
                };
                Ok(InteractionEvent {
                    ts: UnixNanos(row.ts),
                    query_shape: QueryShape::parse(&row.query_shape)
                        .ok_or_else(|| invalid("query shape", &row.query_shape))?,
                    action: InteractionAction::parse(&row.action)
                        .ok_or_else(|| invalid("action", &row.action))?,
                    query_hash: row.query_hash,
                    object_id: row.object_id.map(ObjectId),
                    source_id: row.source_id.map(SourceId),
                    presented_rank: row.presented_rank,
                    session_id: row.session_id,
                })
            })
            .collect()
    }

    pub fn interaction_stats(&self) -> Result<InteractionStats> {
        self.with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*), COUNT(DISTINCT session_id), MIN(ts), MAX(ts)
                 FROM interaction_events",
                [],
                |r| {
                    Ok(InteractionStats {
                        events: r.get::<_, i64>(0)?.max(0) as u64,
                        sessions: r.get::<_, i64>(1)?.max(0) as u64,
                        oldest_ts: r.get::<_, Option<i64>>(2)?.map(UnixNanos),
                        newest_ts: r.get::<_, Option<i64>>(3)?.map(UnixNanos),
                    })
                },
            )?)
        })
    }
}

fn prune_within(conn: &rusqlite::Connection, retention: InteractionRetention) -> Result<u64> {
    let cutoff = UnixNanos::now()
        .0
        .saturating_sub(retention.max_age.as_nanos().min(i64::MAX as u128) as i64);
    let by_age = conn.execute(
        "DELETE FROM interaction_events WHERE ts < ?1",
        params![cutoff],
    )?;
    // The row at offset `max_rows` counting back from the newest is the newest
    // row that must go; everything at or below its id is older. When the table
    // holds no more than the cap the subquery is NULL and nothing matches.
    let by_cap = conn.execute(
        "DELETE FROM interaction_events WHERE id <= (
             SELECT id FROM interaction_events ORDER BY id DESC LIMIT 1 OFFSET ?1
         )",
        params![retention.max_rows as i64],
    )?;
    Ok((by_age + by_cap) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog does not depend on the query parser, so shape tests build
    /// the AST the parser would produce.
    fn text(field: TextField, mode: TextMode) -> Query {
        Query::Text {
            field,
            mode,
            value: "zephyr".into(),
            case_sensitive: false,
            slop: 0,
        }
    }

    #[test]
    fn shape_takes_the_strongest_clause() {
        let name = text(TextField::Name, TextMode::Substring);
        let ranked = text(TextField::Content, TextMode::Ranked);
        let regex = text(TextField::Content, TextMode::Regex);
        assert_eq!(QueryShape::classify(&Query::All), QueryShape::Metadata);
        assert_eq!(
            QueryShape::classify(&Query::extension(&["md"])),
            QueryShape::Metadata
        );
        assert_eq!(
            QueryShape::classify(&Query::and(vec![Query::extension(&["md"]), name.clone()])),
            QueryShape::Name
        );
        assert_eq!(
            QueryShape::classify(&Query::and(vec![name.clone(), ranked.clone()])),
            QueryShape::ContentRanked
        );
        // The strongest clause wins wherever it sits in the tree.
        assert_eq!(
            QueryShape::classify(&Query::and(vec![
                ranked,
                Query::Not {
                    clause: Box::new(regex)
                },
                name
            ])),
            QueryShape::ContentRegex
        );
    }

    #[test]
    fn query_hash_ignores_incidental_whitespace_and_separates_queries() {
        assert_eq!(
            query_hash("  ext:md   readme "),
            query_hash("ext:md readme")
        );
        assert_ne!(query_hash("ext:md readme"), query_hash("readme ext:md"));
        assert_eq!(query_hash("ext:md").len(), 32);
    }

    #[test]
    fn actions_and_shapes_round_trip_through_their_stored_labels() {
        for action in [
            InteractionAction::Presented,
            InteractionAction::OpenedPreview,
            InteractionAction::OpenedFile,
            InteractionAction::CopiedPath,
            InteractionAction::Exported,
        ] {
            assert_eq!(InteractionAction::parse(action.as_str()), Some(action));
        }
        for shape in [
            QueryShape::Metadata,
            QueryShape::Name,
            QueryShape::ContentRanked,
            QueryShape::ContentRegex,
        ] {
            assert_eq!(QueryShape::parse(shape.as_str()), Some(shape));
        }
        assert_eq!(InteractionAction::parse("opened"), None);
        assert_eq!(QueryShape::parse("content"), None);
    }
}
