use crate::schema::ObservationRecord;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_NS: i64 = 86_400_000_000_000;

#[derive(Debug, Clone, Copy)]
pub struct SpoolLimits {
    pub detailed_max_bytes: u64,
    pub detailed_max_age_ns: i64,
    pub summary_max_age_ns: i64,
}

impl Default for SpoolLimits {
    fn default() -> Self {
        Self {
            detailed_max_bytes: 10 * 1024 * 1024 * 1024,
            detailed_max_age_ns: 14 * DAY_NS,
            summary_max_age_ns: 90 * DAY_NS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolStats {
    pub records: u64,
    pub detailed_records: u64,
    pub detailed_bytes: u64,
    pub oldest_utc_ns: Option<i64>,
    pub newest_utc_ns: Option<i64>,
}

pub struct Spool {
    connection: Connection,
    limits: SpoolLimits,
}

impl Spool {
    pub fn open(file: &Path, limits: SpoolLimits) -> rusqlite::Result<Self> {
        let connection = Connection::open(file)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS observations (
               sequence INTEGER PRIMARY KEY AUTOINCREMENT,
               utc_ns INTEGER NOT NULL,
               detailed INTEGER NOT NULL,
               bytes INTEGER NOT NULL,
               body BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS observations_age
               ON observations(detailed, utc_ns, sequence);",
        )?;
        Ok(Self { connection, limits })
    }

    pub fn append(&mut self, record: &ObservationRecord) -> Result<(), SpoolError> {
        let body = serde_json::to_vec(record)?;
        let detailed = record.is_detailed();
        let event_utc_ns = record.utc_ns();
        // Retention follows ingestion time, not an event-controlled clock. A
        // replayed, future-dated, or out-of-order anchor cannot move the ring's
        // cutoff and delete otherwise valid history.
        let retention_utc_ns = utc_now_ns();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO observations(utc_ns, detailed, bytes, body) VALUES (?1, ?2, ?3, ?4)",
            params![event_utc_ns, detailed, body.len() as i64, body],
        )?;
        transaction.execute(
            "DELETE FROM observations WHERE detailed = 1 AND utc_ns < ?1",
            [retention_utc_ns.saturating_sub(self.limits.detailed_max_age_ns)],
        )?;
        transaction.execute(
            "DELETE FROM observations WHERE detailed = 0 AND utc_ns < ?1",
            [retention_utc_ns.saturating_sub(self.limits.summary_max_age_ns)],
        )?;
        let byte_limit = i64::try_from(self.limits.detailed_max_bytes).unwrap_or(i64::MAX);
        prune_detailed_bytes(&transaction, byte_limit)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn records(&self) -> Result<Vec<ObservationRecord>, SpoolError> {
        let mut statement = self
            .connection
            .prepare("SELECT body FROM observations ORDER BY sequence")?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| Ok(serde_json::from_slice(&row?)?)).collect()
    }

    pub fn stats(&self) -> rusqlite::Result<SpoolStats> {
        self.connection.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN detailed = 1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN detailed = 1 THEN bytes ELSE 0 END), 0),
                    MIN(utc_ns), MAX(utc_ns)
             FROM observations",
            [],
            |row| {
                Ok(SpoolStats {
                    records: row.get::<_, i64>(0)? as u64,
                    detailed_records: row.get::<_, i64>(1)? as u64,
                    detailed_bytes: row.get::<_, i64>(2)? as u64,
                    oldest_utc_ns: row.get(3)?,
                    newest_utc_ns: row.get(4)?,
                })
            },
        )
    }

    pub fn latest(&self) -> Result<Option<ObservationRecord>, SpoolError> {
        let body: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT body FROM observations ORDER BY sequence DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        body.map(|value| serde_json::from_slice(&value).map_err(Into::into))
            .transpose()
    }
}

fn utc_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn prune_detailed_bytes(
    transaction: &rusqlite::Transaction<'_>,
    byte_limit: i64,
) -> rusqlite::Result<()> {
    let total: i64 = transaction.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM observations WHERE detailed = 1",
        [],
        |row| row.get(0),
    )?;
    let excess = total.saturating_sub(byte_limit);
    if excess == 0 {
        return Ok(());
    }

    let cutoff = {
        let mut statement = transaction.prepare(
            "SELECT sequence, bytes FROM observations
             WHERE detailed = 1 ORDER BY sequence",
        )?;
        let mut rows = statement.query([])?;
        let mut removed_bytes = 0i64;
        let mut cutoff = None;
        while removed_bytes < excess {
            let Some(row) = rows.next()? else { break };
            cutoff = Some(row.get::<_, i64>(0)?);
            removed_bytes = removed_bytes.saturating_add(row.get::<_, i64>(1)?);
        }
        cutoff
    };
    if let Some(sequence) = cutoff {
        transaction.execute(
            "DELETE FROM observations WHERE detailed = 1 AND sequence <= ?1",
            [sequence],
        )?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::StudyKey;
    use crate::schema::*;

    fn change(utc_ns: i64, object: &str) -> ObservationRecord {
        let key = StudyKey::from_bytes([4; 32]);
        ObservationRecord::LogicalChange(LogicalChange {
            at: TimeAnchor {
                monotonic_ns: 1,
                utc_ns,
            },
            object: key.token("object", object.as_bytes()),
            subtree: key.token("subtree", b"synthetic"),
            operation: ChangeOperation::Update,
            rename_pair: None,
            size: SizeBucket::B1K,
            extension: ExtensionBucket::Source,
            depth: DepthBucket::Shallow,
            edit_count: CountBucket::One,
            delete_recreate_age: None,
            fan_out: CountBucket::One,
            backlog_age: AgeBucket::Immediate,
        })
    }

    #[test]
    fn ring_enforces_byte_and_age_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let now = utc_now_ns();
        let mut spool = Spool::open(
            &temp.path().join("spool.db"),
            SpoolLimits {
                detailed_max_bytes: 500,
                detailed_max_age_ns: 1_000_000_000,
                summary_max_age_ns: 10_000_000_000,
            },
        )
        .unwrap();
        for sequence in 0..20 {
            spool
                .append(&change(
                    now - 1_900_000_000 + sequence * 100_000_000,
                    &format!("token-{sequence:02}"),
                ))
                .unwrap();
        }
        let stats = spool.stats().unwrap();
        assert!(stats.detailed_bytes <= 500);
        assert!(stats.oldest_utc_ns.unwrap() >= now - 1_000_000_000);
        assert!(stats.detailed_records < 20);
    }

    #[test]
    fn event_timestamps_cannot_move_the_retention_cutoff() {
        let temp = tempfile::tempdir().unwrap();
        let now = utc_now_ns();
        let mut spool = Spool::open(
            &temp.path().join("spool.db"),
            SpoolLimits {
                detailed_max_bytes: u64::MAX,
                detailed_max_age_ns: 10_000_000_000,
                summary_max_age_ns: 10_000_000_000,
            },
        )
        .unwrap();
        spool.append(&change(now, "current")).unwrap();
        spool.append(&change(i64::MAX, "future-anchor")).unwrap();
        spool
            .append(&change(now - 500_000_000, "out-of-order"))
            .unwrap();
        assert_eq!(spool.stats().unwrap().detailed_records, 3);
    }

    #[test]
    fn oversized_byte_limit_does_not_wrap() {
        let temp = tempfile::tempdir().unwrap();
        let mut spool = Spool::open(
            &temp.path().join("spool.db"),
            SpoolLimits {
                detailed_max_bytes: u64::MAX,
                detailed_max_age_ns: i64::MAX,
                summary_max_age_ns: i64::MAX,
            },
        )
        .unwrap();
        spool.append(&change(utc_now_ns(), "retained")).unwrap();
        assert_eq!(spool.stats().unwrap().detailed_records, 1);
    }
}
