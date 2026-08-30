//! `POST /api/interactions` — data-collection capture of what a search
//! presented and what happened next.
//!
//! The endpoint is deliberately inert: it never reads back, never influences
//! ranking, and its failures are invisible to the caller. Clients batch their
//! events and post them in the background, so the contract is "took it, now go
//! away" rather than "stored it".
//!
//! Three properties keep it that way:
//!
//! - **The query text is not stored.** Clients send the query they ran; this
//!   module parses it once per distinct text in the batch, hashes the
//!   canonical rendering, labels its shape, and keeps only those two. The text
//!   never reaches the catalog.
//! - **The timestamp is ours.** A client clock cannot place rows in the future
//!   where the retention window would never reach them.
//! - **The write is bounded and detached.** A small semaphore caps how many
//!   batches may be waiting on the catalog writer; past that, batches are
//!   dropped and reported as dropped. A response never waits for the writer.

use crate::api::{ApiError, ApiResult};
use crate::api_json::ApiJson;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use eidos_catalog::interactions::{
    query_hash, InteractionAction, InteractionEvent, QueryShape, MAX_SESSION_ID_LEN,
};
use eidos_domain::{ObjectId, Query, SourceId, UnixNanos};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use ts_rs::TS;

/// Events one request may carry. Clients flush more often rather than bigger.
pub const MAX_INTERACTION_BATCH: usize = 500;
/// Query text longer than this is not worth hashing; the batch is rejected.
const MAX_QUERY_TEXT: usize = 8192;
/// Batches that may be waiting on the catalog writer at once.
pub const MAX_PENDING_INTERACTION_WRITES: usize = 2;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/interactions", post(record_interactions))
}

/// Body of `POST /api/interactions`.
#[derive(Debug, Deserialize, TS)]
pub struct InteractionBatch {
    pub events: Vec<InteractionEventBody>,
}

/// One interaction. `ts` is assigned by the server, so it is absent here.
#[derive(Debug, Deserialize, TS)]
#[ts(optional_fields = nullable)]
pub struct InteractionEventBody {
    /// Opaque per-tab (web) or per-invocation (CLI) id. Never a user id.
    pub session_id: String,
    /// `presented`, `opened_preview`, `opened_file`, `copied_path`, `exported`.
    pub action: String,
    /// The query the results came from. Hashed and discarded, never stored.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub object_id: Option<ObjectId>,
    #[serde(default)]
    pub source_id: Option<SourceId>,
    /// 0-based position of the hit in the result set it was presented in.
    #[serde(default)]
    pub presented_rank: Option<u32>,
}

/// What became of a batch. `dropped` is not an error: a client that is told
/// its events were dropped should carry on exactly as if they were kept.
#[derive(Debug, Default, Serialize, TS)]
pub struct InteractionAck {
    pub accepted: u32,
    pub dropped: u32,
}

async fn record_interactions(
    State(st): State<Arc<AppState>>,
    Json(batch): Json<InteractionBatch>,
) -> ApiResult<InteractionAck> {
    if batch.events.len() > MAX_INTERACTION_BATCH {
        return Err(ApiError::bad_request(format!(
            "interaction batch has {} events; limit is {MAX_INTERACTION_BATCH}",
            batch.events.len()
        )));
    }
    let events = convert(batch.events)?;
    let count = events.len() as u32;
    if count == 0 {
        return Ok(ApiJson(InteractionAck::default()));
    }
    // Fire and forget: the response must not wait behind a scan holding the
    // catalog writer, and a full gate costs the batch rather than the caller.
    let Ok(permit) = st.interaction_writes.clone().try_acquire_owned() else {
        tracing::debug!(count, "dropped an interaction batch; writer gate is full");
        return Ok(ApiJson(InteractionAck {
            accepted: 0,
            dropped: count,
        }));
    };
    let catalog = st.catalog.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Err(e) = catalog.record_interactions(&events) {
            tracing::warn!(error = %e, count, "recording interactions failed");
        }
    });
    Ok(ApiJson(InteractionAck {
        accepted: count,
        dropped: 0,
    }))
}

/// Validate the batch and turn each event into its stored form. Query text is
/// resolved once per distinct string: a page of hits shares one query, so a
/// 500-event batch normally parses once.
fn convert(bodies: Vec<InteractionEventBody>) -> Result<Vec<InteractionEvent>, ApiError> {
    let ts = UnixNanos::now();
    let mut resolved: HashMap<String, (String, QueryShape)> = HashMap::new();
    let mut events = Vec::with_capacity(bodies.len());
    for body in bodies {
        let action = InteractionAction::parse(&body.action).ok_or_else(|| {
            ApiError::bad_request(format!("unknown interaction action '{}'", body.action))
        })?;
        let session_id = body.session_id.trim();
        if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_LEN {
            return Err(ApiError::bad_request(format!(
                "session_id must be 1..={MAX_SESSION_ID_LEN} characters"
            )));
        }
        let q = body.q.unwrap_or_default();
        if q.len() > MAX_QUERY_TEXT {
            return Err(ApiError::bad_request(format!(
                "query text is {} bytes; limit is {MAX_QUERY_TEXT}",
                q.len()
            )));
        }
        let (hash, shape) = resolved
            .entry(q)
            .or_insert_with_key(|text| resolve_query(text))
            .clone();
        events.push(InteractionEvent {
            ts,
            query_hash: hash,
            query_shape: shape,
            object_id: body.object_id,
            source_id: body.source_id,
            presented_rank: body.presented_rank,
            action,
            session_id: session_id.to_string(),
        });
    }
    Ok(events)
}

/// Digest and label one query text.
///
/// A parsed query is hashed by its canonical rendering, so the same query
/// written two ways groups as one. Text the parser rejects still produced a
/// result page for someone, so it is recorded rather than refused: the raw
/// text is hashed as-is and labelled `name`, the shape of the bare words such
/// a query almost always is.
fn resolve_query(text: &str) -> (String, QueryShape) {
    if text.trim().is_empty() {
        return (query_hash(""), QueryShape::Metadata);
    }
    match eidos_query::parse(text) {
        Ok(parsed) => (
            query_hash(&eidos_query::render(&canonical(&parsed.query))),
            QueryShape::classify(&parsed.query),
        ),
        Err(_) => (query_hash(text), QueryShape::Name),
    }
}

/// The query in a form whose rendering does not depend on the order things
/// were typed in.
///
/// `and` and `or` are commutative and their set-valued clauses are sets, so
/// `readme ext:md`, `ext:md readme`, and `ext:md,txt` versus `ext:txt,md` are
/// each one search asked twice. Grouping them apart would split the evidence
/// for a query across as many groups as there are ways to write it. Siblings
/// are ordered by their own canonical rendering, so the sort is bottom-up and
/// stable regardless of nesting.
///
/// Time bounds are floored to the day for the same reason: a relative clause
/// like `mtime:>=30d` parses to the instant it was typed, so without this two
/// runs of one query minutes apart would never group at all.
fn canonical(query: &Query) -> Query {
    /// Whole days since the epoch, in nanoseconds. `div_euclid` floors, so
    /// timestamps before 1970 round the same direction as those after it.
    fn floor_to_day(t: UnixNanos) -> UnixNanos {
        const DAY_NS: i64 = 24 * 60 * 60 * 1_000_000_000;
        UnixNanos(t.0.div_euclid(DAY_NS) * DAY_NS)
    }
    fn ordered(clauses: &[Query]) -> Vec<Query> {
        let mut out: Vec<Query> = clauses.iter().map(canonical).collect();
        out.sort_by_cached_key(eidos_query::render);
        out
    }
    fn sorted<T: Ord + Clone>(values: &[T]) -> Vec<T> {
        let mut out = values.to_vec();
        out.sort();
        out
    }
    fn by_label<T: Copy>(values: &[T], label: impl Fn(T) -> &'static str) -> Vec<T> {
        let mut out = values.to_vec();
        out.sort_by_key(|v| label(*v));
        out
    }
    match query {
        Query::And { clauses } => Query::And {
            clauses: ordered(clauses),
        },
        Query::Or { clauses } => Query::Or {
            clauses: ordered(clauses),
        },
        Query::Not { clause } => Query::Not {
            clause: Box::new(canonical(clause)),
        },
        Query::Extension { values } => Query::Extension {
            values: sorted(values),
        },
        Query::Kind { values } => Query::Kind {
            values: by_label(values, |k| k.as_str()),
        },
        Query::ContentState { states } => Query::ContentState {
            states: by_label(states, |s| s.as_str()),
        },
        Query::Time {
            field,
            after,
            before,
        } => Query::Time {
            field: *field,
            after: after.map(floor_to_day),
            before: before.map(floor_to_day),
        },
        Query::Host { ids, names } => Query::Host {
            ids: sorted(ids),
            names: sorted(names),
        },
        Query::Object { ids } => Query::Object { ids: sorted(ids) },
        Query::Source { ids, names } => Query::Source {
            ids: sorted(ids),
            names: sorted(names),
        },
        leaf => leaf.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_queries_share_a_hash_and_a_shape() {
        let (hash, shape) = resolve_query("ext:md  readme");
        assert_eq!(shape, QueryShape::Name);
        assert_eq!(resolve_query("ext:md readme").0, hash);
        assert_ne!(resolve_query("ext:txt readme").0, hash);
        assert_eq!(resolve_query("content:zephyr").1, QueryShape::ContentRanked);
        assert_eq!(resolve_query("").1, QueryShape::Metadata);
    }

    /// One search written several ways is one group, or the evidence for it
    /// is split across as many groups as there are ways to type it.
    #[test]
    fn clause_order_does_not_split_a_query_into_several_groups() {
        let same = |a: &str, b: &str| {
            assert_eq!(
                resolve_query(a).0,
                resolve_query(b).0,
                "{a} and {b} are the same search"
            );
            assert_eq!(resolve_query(a).1, resolve_query(b).1);
        };
        same("readme ext:md", "ext:md readme");
        same("ext:md,txt", "ext:txt,md");
        same(
            "ext:md mtime:>=30d content:zephyr",
            "content:zephyr ext:md mtime:>=30d",
        );
        same("ext:md OR ext:txt", "ext:txt OR ext:md");
        same("-ext:vhdx size:>1G", "size:>1G -ext:vhdx");
        same("(readme OR license) ext:md", "ext:md (license OR readme)");
        // A relative time clause resolves to the instant it was parsed; the
        // same query run twice must still be one group.
        same("ext:md mtime:>=30d", "mtime:>=30d ext:md");
        // Different searches still land apart.
        assert_ne!(resolve_query("ext:md readme").0, resolve_query("ext:md").0);
        assert_ne!(
            resolve_query("ext:md OR ext:txt").0,
            resolve_query("ext:md ext:txt").0
        );
    }

    #[test]
    fn a_batch_shares_one_resolution_per_distinct_query() {
        let body = |q: &str, action: &str| InteractionEventBody {
            session_id: "s".into(),
            action: action.into(),
            q: Some(q.into()),
            object_id: Some(ObjectId(7)),
            source_id: None,
            presented_rank: Some(0),
        };
        let events = convert(vec![
            body("ext:md", "presented"),
            body("ext:md", "opened_preview"),
            body("content:zephyr", "exported"),
        ])
        .unwrap();
        assert_eq!(events[0].query_hash, events[1].query_hash);
        assert_ne!(events[0].query_hash, events[2].query_hash);
        assert_eq!(events[0].query_shape, QueryShape::Metadata);
        assert_eq!(events[2].query_shape, QueryShape::ContentRanked);
        // One instant for the whole batch, taken here rather than by a client.
        assert_eq!(events[0].ts, events[2].ts);
    }

    #[test]
    fn malformed_events_are_rejected_before_anything_is_written() {
        let bad = |session_id: &str, action: &str| InteractionEventBody {
            session_id: session_id.into(),
            action: action.into(),
            q: None,
            object_id: None,
            source_id: None,
            presented_rank: None,
        };
        assert!(convert(vec![bad("s", "clicked")]).is_err());
        assert!(convert(vec![bad("  ", "presented")]).is_err());
        assert!(convert(vec![bad(&"x".repeat(MAX_SESSION_ID_LEN + 1), "presented")]).is_err());
        assert!(convert(vec![bad("s", "presented")]).is_ok());
    }
}
