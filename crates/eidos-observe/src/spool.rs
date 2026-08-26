use crate::schema::{BundleManifest, ObservationRecord, SCHEMA_VERSION};
use rusqlite::{params, Connection, OptionalExtension};
use std::io::Write;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
    detailed_bytes: i64,
    retention_epoch_utc_ns: i64,
    retention_started: Instant,
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
        let persisted_retention_utc_ns =
            connection.query_row("SELECT MAX(utc_ns) FROM observations", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?;
        let detailed_bytes = connection.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM observations WHERE detailed = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(Self {
            connection,
            limits,
            detailed_bytes,
            // Once the spool has history, continue its persisted logical
            // clock. Adjustable wall time is sampled only for an empty spool;
            // subsequent age advancement uses monotonic process time.
            retention_epoch_utc_ns: persisted_retention_utc_ns.unwrap_or_else(utc_now_ns),
            retention_started: Instant::now(),
        })
    }

    pub fn append(&mut self, record: &ObservationRecord) -> Result<(), SpoolError> {
        let elapsed_ns = self
            .retention_started
            .elapsed()
            .as_nanos()
            .min(i64::MAX as u128) as i64;
        self.append_at(
            record,
            self.retention_epoch_utc_ns.saturating_add(elapsed_ns),
        )
    }

    fn append_at(
        &mut self,
        record: &ObservationRecord,
        retention_utc_ns: i64,
    ) -> Result<(), SpoolError> {
        let body = serde_json::to_vec(record)?;
        let body_bytes = i64::try_from(body.len()).unwrap_or(i64::MAX);
        let detailed = record.is_detailed();
        // Retention follows ingestion time, not an event-controlled clock. A
        // replayed, future-dated, or out-of-order anchor cannot move the ring's
        // cutoff and delete otherwise valid history.
        let transaction = self.connection.transaction()?;
        let mut detailed_bytes = self.detailed_bytes;
        transaction.execute(
            "INSERT INTO observations(utc_ns, detailed, bytes, body) VALUES (?1, ?2, ?3, ?4)",
            params![retention_utc_ns, detailed, body_bytes, body],
        )?;
        if detailed {
            detailed_bytes = detailed_bytes.saturating_add(body_bytes);
        }
        let detailed_cutoff = retention_utc_ns.saturating_sub(self.limits.detailed_max_age_ns);
        let expired_detailed_bytes: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM observations
             WHERE detailed = 1 AND utc_ns < ?1",
            [detailed_cutoff],
            |row| row.get(0),
        )?;
        transaction.execute(
            "DELETE FROM observations WHERE detailed = 1 AND utc_ns < ?1",
            [detailed_cutoff],
        )?;
        detailed_bytes = detailed_bytes.saturating_sub(expired_detailed_bytes);
        transaction.execute(
            "DELETE FROM observations WHERE detailed = 0 AND utc_ns < ?1",
            [retention_utc_ns.saturating_sub(self.limits.summary_max_age_ns)],
        )?;
        let byte_limit = i64::try_from(self.limits.detailed_max_bytes).unwrap_or(i64::MAX);
        let removed_bytes = prune_detailed_bytes(&transaction, detailed_bytes, byte_limit)?;
        detailed_bytes = detailed_bytes.saturating_sub(removed_bytes);
        transaction.commit()?;
        self.detailed_bytes = detailed_bytes;
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
    total: i64,
    byte_limit: i64,
) -> rusqlite::Result<i64> {
    if total <= byte_limit {
        return Ok(0);
    }
    let excess = total.saturating_sub(byte_limit);

    let (cutoff, removed_bytes) = {
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
        (cutoff, removed_bytes)
    };
    if let Some(sequence) = cutoff {
        transaction.execute(
            "DELETE FROM observations WHERE detailed = 1 AND sequence <= ?1",
            [sequence],
        )?;
    }
    Ok(removed_bytes)
}

/// Write an observation bundle straight from a spool file, without going
/// through the collector's live `Spool`.
///
/// The export opens its own read-only connection, so it neither takes the
/// append lock nor blocks collection while it runs — WAL lets the reader see a
/// consistent snapshot beside the writer. Record bodies are already stored
/// serialized, so they stream to the encoder untouched and the export holds
/// one record in memory at a time however large the ring has grown.
pub fn export_bundle(
    spool_file: &Path,
    manifest: &BundleManifest,
    out: &Path,
) -> Result<u64, SpoolError> {
    if manifest.schema != SCHEMA_VERSION {
        return Err(SpoolError::Schema(manifest.schema.clone()));
    }
    let connection = Connection::open_with_flags(
        spool_file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut written = 0u64;
    {
        let output = std::io::BufWriter::new(temporary.as_file_mut());
        let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
        encoder.write_all(b"{\"manifest\":")?;
        serde_json::to_writer(&mut encoder, manifest)?;
        encoder.write_all(b",\"records\":[")?;
        let mut statement =
            connection.prepare("SELECT body FROM observations ORDER BY sequence")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let body: Vec<u8> = row.get(0)?;
            if written > 0 {
                encoder.write_all(b",")?;
            }
            encoder.write_all(&body)?;
            written += 1;
        }
        encoder.write_all(b"]}")?;
        encoder.finish()?.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(out).map_err(|error| error.error)?;
    Ok(written)
}

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("bundle schema {0} is not {SCHEMA_VERSION}")]
    Schema(String),
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
                .append_at(
                    &change(i64::MAX - sequence, &format!("token-{sequence:02}")),
                    now - 1_900_000_000 + sequence * 100_000_000,
                )
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
        spool
            .append_at(
                &change(i64::MAX, "later-future-anchor"),
                now + 20_000_000_000,
            )
            .unwrap();
        assert_eq!(spool.stats().unwrap().detailed_records, 1);
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

    #[test]
    fn reopen_continues_the_persisted_retention_clock() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("spool.db");
        let limits = SpoolLimits {
            detailed_max_bytes: u64::MAX,
            detailed_max_age_ns: 1_000_000_000,
            summary_max_age_ns: 1_000_000_000,
        };
        let mut spool = Spool::open(&file, limits).unwrap();
        spool
            .append_at(&change(i64::MAX, "before-reopen"), 1_000)
            .unwrap();
        drop(spool);

        let mut reopened = Spool::open(&file, limits).unwrap();
        reopened.append(&change(i64::MIN, "after-reopen")).unwrap();
        assert_eq!(reopened.stats().unwrap().detailed_records, 2);
    }

    #[test]
    fn reopen_restores_the_cached_detailed_byte_total() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("spool.db");
        let limits = SpoolLimits {
            detailed_max_bytes: 1_000,
            detailed_max_age_ns: i64::MAX,
            summary_max_age_ns: i64::MAX,
        };
        let mut spool = Spool::open(&file, limits).unwrap();
        spool.append(&change(1, "before-reopen-a")).unwrap();
        spool.append(&change(2, "before-reopen-b")).unwrap();
        drop(spool);

        let mut reopened = Spool::open(&file, limits).unwrap();
        assert_eq!(
            reopened.detailed_bytes as u64,
            reopened.stats().unwrap().detailed_bytes
        );
        reopened.append(&change(3, "after-reopen-a")).unwrap();
        reopened.append(&change(4, "after-reopen-b")).unwrap();
        let stats = reopened.stats().unwrap();
        assert!(stats.detailed_bytes <= limits.detailed_max_bytes);
        assert_eq!(reopened.detailed_bytes as u64, stats.detailed_bytes);
    }
}
