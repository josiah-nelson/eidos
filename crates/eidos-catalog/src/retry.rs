//! Operator retry controls for failed jobs.
//!
//! Transient failures retry on their own with exponential backoff
//! ([`crate::jobs::fail_job`]). Everything else — deterministic, corrupt,
//! unsupported, resource-limit — is terminal by design: it needs a human
//! decision (an extractor fix, a raised limit, a restored share) before the
//! work is worth doing again. This module is that decision.
//!
//! Two sanctioned triggers make a terminal failure eligible again:
//!
//! 1. an explicit operator action, implemented here as
//!    [`Catalog::retry_failed_jobs`];
//! 2. a policy-version change — when `sources.policy_version` is bumped the
//!    decisions that produced the failures no longer apply. Nothing bumps
//!    that column yet; when policy editing lands it should call
//!    [`Catalog::retry_failed_jobs`] with a [`RetrySelector`] for the source
//!    (optionally narrowed by class) as part of the same transaction that
//!    writes the new version.
//!
//! Requeueing preserves history: `attempts`, `last_error`, and
//! `failure_class` stay on the row, `requeue_count`/`requeued_at` record the
//! operator action, and `retry_base_attempts` re-bases the automatic
//! transient budget so a retried job gets a full backoff schedule again.
//!
//! Requeueing is idempotent: a job is only moved out of `failed`, and a
//! candidate is rejected when another job for the same object and stage is
//! already `queued` or `running`, so two retries (or a retry racing a
//! restart requeue) can never leave two active jobs for one object.
//!
//! A retry covers two families, because a content failure can live in
//! either place:
//!
//! - **failed jobs** (`jobs.state = 'failed'`): a transient failure that
//!   exhausted its automatic budget, or a stage that reported a terminal
//!   class through `fail_job`;
//! - **failed content records**: the extraction pipeline writes a
//!   deterministic, corrupt, or resource-limit failure onto the content
//!   record and *finishes* the job, so the queue no longer represents the
//!   object. Those objects go back to `content_state = 'pending'` and their
//!   content job is revived (or re-created) — this is the requeue an
//!   operator needs after fixing an extractor or raising a limit.

use crate::jobs::NewJob;
use crate::{Catalog, Result};
use eidos_domain::{ContentState, FailureClass, JobId, JobStage, ObjectId, SourceId, UnixNanos};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cap on the jobs one bulk action requeues, and on the candidates it
/// examines to find them.
pub const MAX_RETRY_BATCH: u32 = 50_000;
/// Accepted job ids reported back (a bulk retry can accept thousands).
const MAX_REPORTED_IDS: usize = 100;

/// Which failed jobs an operator wants back in the queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrySelector {
    /// One specific job.
    pub job_id: Option<JobId>,
    /// Every job of one object (all generations of the selected stages).
    pub object_id: Option<ObjectId>,
    pub source_id: Option<SourceId>,
    /// Restrict to one stage (`None` = every stage).
    pub stage: Option<JobStage>,
    pub class: Option<FailureClass>,
    /// Match jobs whose `last_error` starts with this text.
    pub reason_prefix: Option<String>,
    /// Stop after requeueing this many. A confirmation can pass the count
    /// its preview reported and be sure the action stays within it.
    pub limit: Option<u32>,
    /// Ignore failures recorded after this instant. A confirmation passes
    /// the `as_of` of the preview it is confirming, so work that failed
    /// between the two steps is left for the next round instead of being
    /// requeued on the strength of a number that did not include it.
    pub as_of: Option<UnixNanos>,
    /// Count what would happen without changing anything.
    pub preview: bool,
}

impl RetrySelector {
    pub fn job(id: JobId) -> Self {
        Self {
            job_id: Some(id),
            ..Default::default()
        }
    }

    pub fn source(id: SourceId, stage: JobStage) -> Self {
        Self {
            source_id: Some(id),
            stage: Some(stage),
            ..Default::default()
        }
    }

    /// A selector naming one job or one object addresses rows the operator
    /// can see individually, so non-failed states are reported as rejected
    /// instead of silently not matching.
    fn is_explicit(&self) -> bool {
        self.job_id.is_some() || self.object_id.is_some()
    }
}

/// What a retry did (or, in preview mode, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryReport {
    pub preview: bool,
    /// When this evaluation ran; pass it back as `as_of` to confirm exactly
    /// the failures it counted.
    pub as_of: UnixNanos,
    /// Jobs moved back to `queued`, plus failed objects put back to
    /// `pending` with a content job (or what would be, in a preview).
    pub accepted: u64,
    /// Candidates that are no longer worth running (deleted, retired,
    /// superseded by a newer generation, content disabled).
    pub skipped: u64,
    /// Candidates that must not be touched now (running, already queued,
    /// already active under another job).
    pub rejected: u64,
    /// Bytes of the objects behind the accepted work.
    pub bytes: u64,
    pub skipped_reasons: BTreeMap<String, u64>,
    pub rejected_reasons: BTreeMap<String, u64>,
    /// Up to 100 accepted job ids, for logs and single-job callers.
    pub job_ids: Vec<JobId>,
}

impl RetryReport {
    fn accept(&mut self, id: Option<JobId>, bytes: u64) {
        self.accepted += 1;
        self.bytes += bytes;
        if let Some(id) = id {
            if self.job_ids.len() < MAX_REPORTED_IDS {
                self.job_ids.push(id);
            }
        }
    }

    fn skip(&mut self, reason: &str) {
        self.skipped += 1;
        *self.skipped_reasons.entry(reason.to_string()).or_default() += 1;
    }

    fn reject(&mut self, reason: &str) {
        self.rejected += 1;
        *self.rejected_reasons.entry(reason.to_string()).or_default() += 1;
    }

    pub fn total(&self) -> u64 {
        self.accepted + self.skipped + self.rejected
    }
}

/// Escape SQL `LIKE` wildcards so a reason prefix is matched literally.
fn like_prefix(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// One candidate row plus the object/source facts needed to judge it.
struct Candidate {
    job_id: i64,
    state: String,
    object_id: Option<i64>,
    object_generation: i64,
    stage: String,
    estimated_cost: i64,
    attempts: i64,
    object_deleted: bool,
    object_generation_now: Option<i64>,
    source_state: String,
    content_enabled: bool,
}

const CANDIDATE_SQL: &str = "SELECT j.job_id, j.state, j.object_id, j.object_generation, j.stage, j.estimated_cost, j.attempts,
        o.object_id IS NULL OR o.deleted_at IS NOT NULL, o.generation, COALESCE(s.state, ''), COALESCE(s.content_enabled, 0)
     FROM jobs j
     LEFT JOIN objects o ON o.object_id = j.object_id
     LEFT JOIN sources s ON s.source_id = j.source_id
     WHERE (?1 IS NULL OR j.job_id = ?1)
       AND (?2 IS NULL OR j.object_id = ?2)
       AND (?3 IS NULL OR j.source_id = ?3)
       AND (?4 IS NULL OR j.stage = ?4)
       AND (?5 IS NULL OR j.failure_class = ?5)
       AND (?6 IS NULL OR j.last_error LIKE ?6 ESCAPE '\\')
       AND (?7 = 1 OR j.state = 'failed')
       AND (?8 IS NULL OR j.finished_at IS NULL OR j.finished_at <= ?8)
     ORDER BY j.job_id
     LIMIT ?9";

impl Catalog {
    /// Requeue the failed work a selector names: failed jobs, and (for the
    /// content stage) objects whose extraction failed for good.
    ///
    /// The action runs in one immediate transaction, so the
    /// duplicate-active check and the requeue cannot interleave with a
    /// worker claim. A preview applies exactly the same rules, writes
    /// nothing, and runs on a pooled reader: counting a large backlog must
    /// not hold the single catalog writer away from the workers.
    pub fn retry_failed_jobs(&self, sel: &RetrySelector) -> Result<RetryReport> {
        let limit = u64::from(sel.limit.unwrap_or(MAX_RETRY_BATCH).min(MAX_RETRY_BATCH));
        let prefix = sel.reason_prefix.as_deref().map(like_prefix);
        let mut report = RetryReport {
            preview: sel.preview,
            as_of: UnixNanos::now(),
            ..Default::default()
        };
        if sel.preview {
            self.with_reader(|conn| evaluate(conn, sel, &prefix, limit, &mut report))?;
        } else {
            self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                evaluate(&tx, sel, &prefix, limit, &mut report)?;
                tx.commit()?;
                Ok(())
            })?;
        }
        Ok(report)
    }

    /// Retryable failures per source for one stage: `(count, bytes)` over
    /// both families a retry covers, so the bulk form can offer itself.
    pub fn failed_work_by_source(&self, stage: JobStage) -> Result<BTreeMap<SourceId, (u64, u64)>> {
        self.with_reader(|conn| {
            let mut out: BTreeMap<SourceId, (u64, u64)> = BTreeMap::new();
            let mut add = |r: &rusqlite::Row<'_>| -> rusqlite::Result<()> {
                let e = out.entry(SourceId(r.get(0)?)).or_default();
                e.0 += r.get::<_, i64>(1)? as u64;
                e.1 += r.get::<_, i64>(2)?.max(0) as u64;
                Ok(())
            };
            let mut stmt = conn.prepare_cached(
                "SELECT source_id, COUNT(*), COALESCE(SUM(estimated_cost), 0) FROM jobs
                 WHERE state = 'failed' AND stage = ?1 GROUP BY source_id",
            )?;
            let mut rows = stmt.query(params![stage.as_str()])?;
            while let Some(r) = rows.next()? {
                add(r)?;
            }
            if stage == JobStage::ContentText {
                let mut stmt = conn.prepare_cached(
                    "SELECT o.source_id, COUNT(*), COALESCE(SUM(o.size), 0) FROM objects o
                     WHERE o.content_state = 'failed' AND o.deleted_at IS NULL
                       AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.object_id = o.object_id
                                       AND j.stage = 'content_text' AND j.state IN ('queued','running','failed'))
                     GROUP BY o.source_id",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    add(r)?;
                }
            }
            Ok(out)
        })
    }
}

/// Classify (and, unless previewing, requeue) everything the selector
/// names. `conn` is a write transaction for an action and a read-only
/// connection for a preview; the preview path never reaches a write.
fn evaluate(
    conn: &rusqlite::Connection,
    sel: &RetrySelector,
    prefix: &Option<String>,
    limit: u64,
    report: &mut RetryReport,
) -> Result<()> {
    let now = report.as_of.0;
    let candidates: Vec<Candidate> = conn
        .prepare(CANDIDATE_SQL)?
        .query_map(
            params![
                sel.job_id.map(|j| j.0),
                sel.object_id.map(|o| o.0),
                sel.source_id.map(|s| s.0),
                sel.stage.map(|s| s.as_str()),
                sel.class.map(|c| c.as_str()),
                prefix,
                sel.is_explicit() as i64,
                sel.as_of.map(|t| t.0),
                MAX_RETRY_BATCH as i64
            ],
            |r| {
                Ok(Candidate {
                    job_id: r.get(0)?,
                    state: r.get(1)?,
                    object_id: r.get(2)?,
                    object_generation: r.get(3)?,
                    stage: r.get(4)?,
                    estimated_cost: r.get(5)?,
                    attempts: r.get(6)?,
                    object_deleted: r.get::<_, i64>(7)? != 0,
                    object_generation_now: r.get(8)?,
                    source_state: r.get(9)?,
                    content_enabled: r.get::<_, i64>(10)? != 0,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    for c in candidates {
        if report.accepted >= limit {
            break;
        }
        match classify(conn, &c)? {
            Verdict::Accept => {
                if !sel.preview {
                    conn.execute(
                        "UPDATE jobs SET state = 'queued', scheduled_at = ?2, started_at = NULL, finished_at = NULL,
                            worker = NULL, requeue_count = requeue_count + 1, requeued_at = ?2, retry_base_attempts = ?3
                         WHERE job_id = ?1 AND state = 'failed'",
                        params![c.job_id, now, c.attempts],
                    )?;
                }
                report.accept(Some(JobId(c.job_id)), c.estimated_cost.max(0) as u64);
            }
            Verdict::Skip(why) => report.skip(why),
            Verdict::Reject(why) => report.reject(why),
        }
    }
    if report.accepted < limit
        && sel.job_id.is_none()
        && sel.stage.is_none_or(|s| s == JobStage::ContentText)
    {
        retry_failed_content(conn, sel, prefix, limit, now, report)?;
    }
    Ok(())
}

/// Objects whose extraction failed for good: the pipeline records a
/// deterministic, corrupt, or resource-limit failure on the content record
/// and finishes the job, so nothing in the queue represents them any more.
/// They come back only through this path, which puts the object back to
/// `pending` and revives (or re-creates) its content job.
const FAILED_CONTENT_SQL: &str = "SELECT o.object_id, o.source_id, o.generation, o.size, COALESCE(s.state, ''),
        COALESCE(s.content_enabled, 0)
     FROM objects o
     JOIN content_records c ON c.object_id = o.object_id
     LEFT JOIN sources s ON s.source_id = o.source_id
     WHERE o.content_state = 'failed' AND o.deleted_at IS NULL AND c.generation = o.generation
       AND (?1 IS NULL OR o.object_id = ?1)
       AND (?2 IS NULL OR o.source_id = ?2)
       AND (?3 IS NULL OR c.failure_class = ?3)
       AND (?4 IS NULL OR c.error LIKE ?4 ESCAPE '\\')
       AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.object_id = o.object_id AND j.stage = 'content_text'
                       AND j.state IN ('queued','running'))
     ORDER BY o.object_id
     LIMIT ?6";

fn retry_failed_content(
    conn: &rusqlite::Connection,
    sel: &RetrySelector,
    prefix: &Option<String>,
    limit: u64,
    now: i64,
    report: &mut RetryReport,
) -> Result<()> {
    struct Row {
        object: ObjectId,
        source: SourceId,
        generation: u32,
        size: u64,
        source_state: String,
        content_enabled: bool,
    }
    let rows: Vec<Row> = conn
        .prepare(FAILED_CONTENT_SQL)?
        .query_map(
            params![
                sel.object_id.map(|o| o.0),
                sel.source_id.map(|s| s.0),
                sel.class.map(|c| c.as_str()),
                prefix,
                sel.as_of.map(|t| t.0),
                MAX_RETRY_BATCH as i64
            ],
            |r| {
                Ok(Row {
                    object: ObjectId(r.get(0)?),
                    source: SourceId(r.get(1)?),
                    generation: r.get::<_, i64>(2)? as u32,
                    size: r.get::<_, i64>(3)?.max(0) as u64,
                    source_state: r.get(4)?,
                    content_enabled: r.get::<_, i64>(5)? != 0,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    for row in rows {
        if report.accepted >= limit {
            break;
        }
        if row.source_state == "retired" {
            report.skip("retired");
            continue;
        }
        if !row.content_enabled {
            report.skip("content_disabled");
            continue;
        }
        let key = NewJob::object_key(JobStage::ContentText, row.object, row.generation);
        let existing: Option<(i64, i64)> = conn
            .query_row(
                "SELECT job_id, attempts FROM jobs WHERE idempotency_key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if !sel.preview {
            match existing {
                // Revive the finished job in place: its attempt and error
                // history is the record of what went wrong.
                Some((job_id, attempts)) => {
                    conn.execute(
                        "UPDATE jobs SET state = 'queued', scheduled_at = ?2, started_at = NULL, finished_at = NULL,
                            worker = NULL, requeue_count = requeue_count + 1, requeued_at = ?2, retry_base_attempts = ?3
                         WHERE job_id = ?1",
                        params![job_id, now, attempts],
                    )?;
                }
                None => crate::content::enqueue_content_for(
                    conn,
                    row.source,
                    row.object,
                    row.generation,
                    row.size,
                )?,
            }
            // The object is a content candidate again; the content record
            // keeps its failure until the re-run overwrites it.
            crate::content::flip_state(conn, row.object, ContentState::Pending, None)?;
        }
        report.accept(existing.map(|(id, _)| JobId(id)), row.size);
    }
    Ok(())
}

enum Verdict {
    Accept,
    Skip(&'static str),
    Reject(&'static str),
}

fn classify(conn: &rusqlite::Connection, c: &Candidate) -> Result<Verdict> {
    if c.state != "failed" {
        return Ok(Verdict::Reject(match c.state.as_str() {
            "running" => "running",
            "queued" => "queued",
            "done" => "done",
            _ => "superseded",
        }));
    }
    if c.source_state == "retired" {
        return Ok(Verdict::Skip("retired"));
    }
    if let Some(obj) = c.object_id {
        if c.object_deleted {
            return Ok(Verdict::Skip("deleted"));
        }
        if c.object_generation_now != Some(c.object_generation) {
            // A later scan already produced (or will produce) a job for the
            // current generation; re-running the old one would index text
            // that no longer exists.
            return Ok(Verdict::Skip("stale_generation"));
        }
        if c.stage == JobStage::ContentText.as_str() && !c.content_enabled {
            return Ok(Verdict::Skip("content_disabled"));
        }
        let active: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM jobs WHERE object_id = ?1 AND stage = ?2
                            AND state IN ('queued','running') AND job_id != ?3)",
            params![obj, c.stage, c.job_id],
            |r| r.get::<_, i64>(0).map(|n| n != 0),
        )?;
        if active {
            return Ok(Verdict::Reject("already_active"));
        }
    }
    Ok(Verdict::Accept)
}
