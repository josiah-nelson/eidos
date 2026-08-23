//! Durable jobs and the derived-index outbox (ARCHITECTURE section 8).
//!
//! Jobs carry source, object generation, stage, priority, estimated cost,
//! retry state, and an idempotency key. Transient failures retry with
//! exponential backoff; deterministic, unsupported, corrupt, and
//! resource-limit failures do not retry. A newer object generation
//! supersedes queued jobs of older generations for the same stage.

use crate::{Catalog, CatalogError, Result};
use eidos_domain::{
    FailureClass, JobId, JobStage, JobState, ObjectId, Priority, SourceId, UnixNanos,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

pub const MAX_TRANSIENT_ATTEMPTS: u32 = 6;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewJob {
    pub source_id: SourceId,
    pub object_id: Option<ObjectId>,
    pub object_generation: u32,
    pub stage: JobStage,
    pub priority: Priority,
    pub idempotency_key: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub estimated_cost: u64,
}

impl NewJob {
    /// Conventional idempotency key for per-object stage work.
    pub fn object_key(stage: JobStage, object: ObjectId, generation: u32) -> String {
        format!("{}:{}:{}", stage.as_str(), object.0, generation)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct JobRecord {
    pub id: JobId,
    pub source_id: SourceId,
    pub object_id: Option<ObjectId>,
    pub object_generation: u32,
    pub stage: JobStage,
    pub priority: Priority,
    pub state: JobState,
    pub attempts: u32,
    pub idempotency_key: String,
    pub payload: Option<serde_json::Value>,
    pub estimated_cost: u64,
    pub created_at: UnixNanos,
    pub scheduled_at: UnixNanos,
    pub started_at: Option<UnixNanos>,
    pub finished_at: Option<UnixNanos>,
    pub worker: Option<String>,
    pub last_error: Option<String>,
    pub failure_class: Option<FailureClass>,
    /// How often an operator has requeued this job (see [`crate::retry`]).
    #[serde(default)]
    pub requeue_count: u32,
    #[serde(default)]
    pub requeued_at: Option<UnixNanos>,
}

const JOB_COLUMNS: &str = "job_id, source_id, object_id, object_generation, stage, priority, state, attempts, idempotency_key, \
     payload, estimated_cost, created_at, scheduled_at, started_at, finished_at, worker, last_error, failure_class, \
     requeue_count, requeued_at";

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    let payload: Option<String> = r.get(9)?;
    Ok(JobRecord {
        id: JobId(r.get(0)?),
        source_id: SourceId(r.get(1)?),
        object_id: r.get::<_, Option<i64>>(2)?.map(ObjectId),
        object_generation: r.get::<_, i64>(3)? as u32,
        stage: JobStage::parse(&r.get::<_, String>(4)?).unwrap_or(JobStage::ContentText),
        priority: Priority::from_u8(r.get::<_, i64>(5)? as u8).unwrap_or(Priority::NormalText),
        state: JobState::parse(&r.get::<_, String>(6)?).unwrap_or(JobState::Queued),
        attempts: r.get::<_, i64>(7)? as u32,
        idempotency_key: r.get(8)?,
        payload: payload.and_then(|p| serde_json::from_str(&p).ok()),
        estimated_cost: r.get::<_, i64>(10)? as u64,
        created_at: UnixNanos(r.get(11)?),
        scheduled_at: UnixNanos(r.get(12)?),
        started_at: r.get::<_, Option<i64>>(13)?.map(UnixNanos),
        finished_at: r.get::<_, Option<i64>>(14)?.map(UnixNanos),
        worker: r.get(15)?,
        last_error: r.get(16)?,
        failure_class: r
            .get::<_, Option<String>>(17)?
            .and_then(|s| FailureClass::parse(&s)),
        requeue_count: r.get::<_, i64>(18)? as u32,
        requeued_at: r.get::<_, Option<i64>>(19)?.map(UnixNanos),
    })
}

/// Enqueue inside an existing transaction/connection. Returns `None` when a
/// job with the same idempotency key already exists in a non-failed state.
pub fn enqueue_conn(conn: &Connection, job: &NewJob) -> Result<Option<JobId>> {
    let now = UnixNanos::now().0;
    let existing: Option<(i64, String)> = conn
        .prepare_cached("SELECT job_id, state FROM jobs WHERE idempotency_key = ?1")?
        .query_row(params![job.idempotency_key], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((id, state)) = existing {
        if state == JobState::Failed.as_str() || state == JobState::Superseded.as_str() {
            conn.execute(
                "UPDATE jobs SET state = 'queued', attempts = 0, retry_base_attempts = 0, scheduled_at = ?2,
                    started_at = NULL, finished_at = NULL, last_error = NULL, failure_class = NULL, priority = ?3
                 WHERE job_id = ?1",
                params![id, now, job.priority as i64],
            )?;
            return Ok(Some(JobId(id)));
        }
        return Ok(None);
    }
    // Supersede older generations of the same object/stage.
    if let Some(obj) = job.object_id {
        conn.execute(
            "UPDATE jobs SET state = 'superseded', finished_at = ?4 WHERE object_id = ?1 AND stage = ?2
               AND object_generation < ?3 AND state IN ('queued')",
            params![obj.0, job.stage.as_str(), job.object_generation as i64, now],
        )?;
    }
    conn.execute(
        "INSERT INTO jobs (source_id, object_id, object_generation, stage, priority, state, idempotency_key, payload,
            estimated_cost, created_at, scheduled_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, ?9)",
        params![
            job.source_id.0,
            job.object_id.map(|o| o.0),
            job.object_generation as i64,
            job.stage.as_str(),
            job.priority as i64,
            job.idempotency_key,
            job.payload.as_ref().map(|p| p.to_string()),
            job.estimated_cost as i64,
            now
        ],
    )?;
    Ok(Some(JobId(conn.last_insert_rowid())))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct JobCounts {
    /// `stage -> state -> count`
    pub by_stage: BTreeMap<String, BTreeMap<String, u64>>,
    pub queued: u64,
    pub running: u64,
    pub failed: u64,
    pub oldest_queued_age_ms: Option<u64>,
}

impl Catalog {
    pub fn enqueue(&self, job: &NewJob) -> Result<Option<JobId>> {
        self.with_writer(|conn| enqueue_conn(conn, job))
    }

    pub fn enqueue_many(&self, jobs: &[NewJob]) -> Result<usize> {
        self.with_writer(|conn| {
            let tx = conn.transaction()?;
            let mut n = 0;
            for j in jobs {
                if enqueue_conn(&tx, j)?.is_some() {
                    n += 1;
                }
            }
            tx.commit()?;
            Ok(n)
        })
    }

    /// Claim the highest-priority due job in any of `stages`.
    pub fn claim_job(&self, stages: &[JobStage], worker: &str) -> Result<Option<JobRecord>> {
        self.claim_job_filtered(stages, worker, &[])
    }

    /// Like `claim_job`, skipping jobs of `exclude_sources` (sources whose
    /// concurrency budget is currently exhausted).
    pub fn claim_job_filtered(
        &self,
        stages: &[JobStage],
        worker: &str,
        exclude_sources: &[SourceId],
    ) -> Result<Option<JobRecord>> {
        Ok(self
            .claim_jobs(stages, worker, exclude_sources, 1)?
            .into_iter()
            .next())
    }

    /// Claim up to `limit` due jobs in one transaction. All claimed jobs
    /// belong to the source of the highest-priority one, so a worker's
    /// batch counts once against that source's concurrency budget.
    pub fn claim_jobs(
        &self,
        stages: &[JobStage],
        worker: &str,
        exclude_sources: &[SourceId],
        limit: u32,
    ) -> Result<Vec<JobRecord>> {
        let mut admit = |source: SourceId| (!exclude_sources.contains(&source)).then_some(());
        Ok(self
            .claim_jobs_admitted(stages, worker, limit, &mut admit)?
            .map(|(_, jobs)| jobs)
            .unwrap_or_default())
    }

    /// Like [`Catalog::claim_jobs`], but the source is admitted by `admit`
    /// from inside the claiming transaction, before a single job is marked
    /// `running`.
    ///
    /// This is how per-source concurrency is enforced: `admit` reserves
    /// capacity and returns a guard, so the check and the claim are one
    /// atomic step and two workers cannot both act on the same free slot.
    /// A source `admit` rejects is skipped and the next eligible source is
    /// offered instead, so a saturated source never starves the others.
    /// Returns `None` when no source has due work it will admit; on any
    /// error the guard is dropped before returning, releasing its capacity.
    pub fn claim_jobs_admitted<T>(
        &self,
        stages: &[JobStage],
        worker: &str,
        limit: u32,
        admit: &mut dyn FnMut(SourceId) -> Option<T>,
    ) -> Result<Option<(T, Vec<JobRecord>)>> {
        if stages.is_empty() || limit == 0 {
            return Ok(None);
        }
        let stage_list = stages
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = UnixNanos::now().0;
            let mut rejected: Vec<SourceId> = Vec::new();
            let (permit, first) = loop {
                let exclude = if rejected.is_empty() {
                    String::new()
                } else {
                    format!(
                        " AND source_id NOT IN ({})",
                        rejected
                            .iter()
                            .map(|s| s.0.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                let sql = format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'queued' AND stage IN ({stage_list}) AND scheduled_at <= ?1{exclude}
                     ORDER BY priority ASC, scheduled_at ASC, job_id ASC LIMIT 1"
                );
                let first = match tx.query_row(&sql, params![now], job_from_row).optional()? {
                    Some(j) => j,
                    None => return Ok(None),
                };
                match admit(first.source_id) {
                    Some(permit) => break (permit, first),
                    // No capacity on that source right now: look past it.
                    None => rejected.push(first.source_id),
                }
            };
            let mut jobs = vec![first];
            if limit > 1 {
                let more_sql = format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'queued' AND stage IN ({stage_list}) AND scheduled_at <= ?1
                       AND source_id = ?2 AND job_id != ?3
                     ORDER BY priority ASC, scheduled_at ASC, job_id ASC LIMIT ?4"
                );
                let more: Vec<JobRecord> = tx
                    .prepare(&more_sql)?
                    .query_map(
                        params![now, jobs[0].source_id.0, jobs[0].id.0, (limit - 1) as i64],
                        job_from_row,
                    )?
                    .collect::<rusqlite::Result<_>>()?;
                jobs.extend(more);
            }
            {
                let mut upd = tx.prepare_cached(
                    "UPDATE jobs SET state = 'running', attempts = attempts + 1, started_at = ?2, worker = ?3 WHERE job_id = ?1",
                )?;
                for j in &jobs {
                    upd.execute(params![j.id.0, now, worker])?;
                }
            }
            tx.commit()?;
            Ok(Some((
                permit,
                jobs.into_iter()
                    .map(|job| JobRecord {
                        state: JobState::Running,
                        attempts: job.attempts + 1,
                        started_at: Some(UnixNanos(now)),
                        worker: Some(worker.to_string()),
                        ..job
                    })
                    .collect(),
            )))
        })
    }

    /// Drop a job entirely (used when its source's content policy was
    /// disabled after it was queued, so re-enabling re-queues it).
    pub fn delete_job(&self, id: JobId) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute("DELETE FROM jobs WHERE job_id = ?1", params![id.0])?;
            Ok(())
        })
    }

    /// Queued + running job counts per source for one stage.
    pub fn jobs_by_source(&self, stage: JobStage) -> Result<BTreeMap<SourceId, (u64, u64)>> {
        self.with_reader(|conn| {
            let mut out: BTreeMap<SourceId, (u64, u64)> = BTreeMap::new();
            let mut stmt = conn.prepare_cached(
                "SELECT source_id, state, COUNT(*) FROM jobs WHERE stage = ?1 AND state IN ('queued','running') GROUP BY source_id, state",
            )?;
            let rows = stmt.query_map(params![stage.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64))
            })?;
            for row in rows {
                let (sid, state, n) = row?;
                let e = out.entry(SourceId(sid)).or_default();
                if state == "queued" {
                    e.0 += n;
                } else {
                    e.1 += n;
                }
            }
            Ok(out)
        })
    }

    /// Queued and running counts for one source and stage. Scheduler paths
    /// use this focused query instead of rebuilding the all-source activity
    /// summary for every candidate source.
    pub fn active_job_counts(&self, source: SourceId, stage: JobStage) -> Result<(u64, u64)> {
        self.with_reader(|conn| {
            let mut counts = (0, 0);
            let mut stmt = conn.prepare_cached(
                "SELECT state, COUNT(*) FROM jobs WHERE source_id = ?1 AND stage = ?2 AND state IN ('queued','running') GROUP BY state",
            )?;
            let rows = stmt.query_map(params![source.0, stage.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?;
            for row in rows {
                let (state, count) = row?;
                if state == "queued" {
                    counts.0 += count;
                } else {
                    counts.1 += count;
                }
            }
            Ok(counts)
        })
    }

    pub fn complete_job(&self, id: JobId) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE jobs SET state = 'done', finished_at = ?2, last_error = NULL WHERE job_id = ?1",
                params![id.0, UnixNanos::now().0],
            )?;
            Ok(())
        })
    }

    /// Record a failure. Transient failures are re-queued with exponential
    /// backoff up to `MAX_TRANSIENT_ATTEMPTS`. The budget counts attempts
    /// since the last operator requeue, so `attempts` keeps the full history
    /// while an explicit retry restores the automatic backoff schedule.
    pub fn fail_job(&self, id: JobId, class: FailureClass, error: &str) -> Result<JobState> {
        self.with_writer(|conn| {
            let now = UnixNanos::now();
            let (total, base): (i64, i64) = conn
                .query_row(
                    "SELECT attempts, retry_base_attempts FROM jobs WHERE job_id = ?1",
                    params![id.0],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| CatalogError::NotFound(format!("job {id}")))?;
            let attempts = (total - base).max(0);
            let retry = class.retryable() && (attempts as u32) < MAX_TRANSIENT_ATTEMPTS;
            let state = if retry { JobState::Queued } else { JobState::Failed };
            let backoff_s = 10i64.saturating_mul(1i64 << attempts.min(10));
            let scheduled = if retry {
                now.0 + backoff_s * 1_000_000_000
            } else {
                now.0
            };
            conn.execute(
                "UPDATE jobs SET state = ?2, scheduled_at = ?3, finished_at = CASE WHEN ?2 = 'failed' THEN ?4 ELSE NULL END,
                    last_error = ?5, failure_class = ?6 WHERE job_id = ?1",
                params![id.0, state.as_str(), scheduled, now.0, error, class.as_str()],
            )?;
            Ok(state)
        })
    }

    pub fn get_job(&self, id: JobId) -> Result<Option<JobRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE job_id = ?1"))?
                .query_row(params![id.0], job_from_row)
                .optional()?)
        })
    }

    pub fn job_counts(&self, source: Option<SourceId>) -> Result<JobCounts> {
        self.with_reader(|conn| {
            let mut out = JobCounts::default();
            let mut stmt = conn.prepare_cached(
                "SELECT stage, state, COUNT(*) FROM jobs WHERE (?1 IS NULL OR source_id = ?1) GROUP BY stage, state",
            )?;
            let rows = stmt.query_map(params![source.map(|s| s.0)], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64))
            })?;
            for row in rows {
                let (stage, state, n) = row?;
                match state.as_str() {
                    "queued" => out.queued += n,
                    "running" => out.running += n,
                    "failed" => out.failed += n,
                    _ => {}
                }
                out.by_stage.entry(stage).or_default().insert(state, n);
            }
            let oldest: Option<i64> = conn
                .query_row(
                    "SELECT MIN(scheduled_at) FROM jobs WHERE state = 'queued' AND (?1 IS NULL OR source_id = ?1)",
                    params![source.map(|s| s.0)],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            out.oldest_queued_age_ms = oldest.map(|t| (UnixNanos::now().as_millis() - t / 1_000_000).max(0) as u64);
            Ok(out)
        })
    }

    pub fn recent_failed_jobs(&self, limit: u32) -> Result<Vec<JobRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'failed' ORDER BY finished_at DESC LIMIT ?1"
                ))?
                .query_map(params![limit as i64], job_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Jobs left `running` by a crash are re-queued at startup.
    pub fn requeue_running_jobs(&self) -> Result<u64> {
        self.with_writer(|conn| {
            let n = conn.execute(
                "UPDATE jobs SET state = 'queued', scheduled_at = ?1, worker = NULL WHERE state = 'running'",
                params![UnixNanos::now().0],
            )?;
            Ok(n as u64)
        })
    }

    /// Delete finished jobs older than `age`.
    pub fn prune_jobs(&self, age: std::time::Duration) -> Result<u64> {
        self.with_writer(|conn| {
            let cutoff = UnixNanos::now().0 - age.as_nanos() as i64;
            let n = conn.execute(
                "DELETE FROM jobs WHERE state IN ('done','superseded') AND finished_at < ?1",
                params![cutoff],
            )?;
            Ok(n as u64)
        })
    }

    // ----- outbox ----------------------------------------------------------

    pub fn outbox_poll(&self, after_seq: i64, limit: u32) -> Result<Vec<OutboxRow>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT seq, source_id, object_id, op, generation, created_at FROM outbox
                     WHERE consumed_at IS NULL AND seq > ?1 ORDER BY seq LIMIT ?2",
                )?
                .query_map(params![after_seq, limit as i64], |r| {
                    Ok(OutboxRow {
                        seq: r.get(0)?,
                        source_id: SourceId(r.get(1)?),
                        object_id: ObjectId(r.get(2)?),
                        op: r.get(3)?,
                        generation: r.get(4)?,
                        created_at: UnixNanos(r.get(5)?),
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn outbox_consume(&self, up_to_seq: i64) -> Result<u64> {
        self.with_writer(|conn| {
            let n = conn.execute(
                "UPDATE outbox SET consumed_at = ?2 WHERE consumed_at IS NULL AND seq <= ?1",
                params![up_to_seq, UnixNanos::now().0],
            )?;
            Ok(n as u64)
        })
    }

    pub fn outbox_pending(&self) -> Result<u64> {
        self.with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM outbox WHERE consumed_at IS NULL",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }
}

pub fn outbox_append_conn(
    conn: &Connection,
    source: SourceId,
    object: ObjectId,
    op: &str,
    generation: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO outbox (source_id, object_id, op, generation, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![source.0, object.0, op, generation, UnixNanos::now().0])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRow {
    pub seq: i64,
    pub source_id: SourceId,
    pub object_id: ObjectId,
    pub op: String,
    pub generation: i64,
    pub created_at: UnixNanos,
}
