//! Catalog schema and monotonic migrations.
//!
//! Migrations are applied in order inside a transaction and tracked both by
//! `PRAGMA user_version` and the `schema_migrations` table. Large mechanical
//! reindexing belongs to derived indexes, not to catalog migrations.

use rusqlite::Connection;

pub const CURRENT_VERSION: i64 = MIGRATIONS.len() as i64;

/// `(description, sql)` pairs; index + 1 is the resulting `user_version`.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "initial catalog schema",
        r#"
CREATE TABLE schema_migrations (
    version     INTEGER PRIMARY KEY,
    description TEXT NOT NULL,
    applied_at  INTEGER NOT NULL
);

CREATE TABLE hosts (
    host_id    INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    platform   TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE volumes (
    volume_id         INTEGER PRIMARY KEY,
    host_id           INTEGER NOT NULL,
    volume_serial     INTEGER NOT NULL,
    filesystem        TEXT NOT NULL DEFAULT '',
    volume_name       TEXT NOT NULL DEFAULT '',
    drive_type        TEXT NOT NULL DEFAULT 'unknown',
    fs_flags          INTEGER NOT NULL DEFAULT 0,
    bytes_per_cluster INTEGER NOT NULL DEFAULT 0,
    volume_root       TEXT NOT NULL DEFAULT '',
    supports_usn      INTEGER NOT NULL DEFAULT 0,
    supports_file_ids INTEGER NOT NULL DEFAULT 0,
    updated_at        INTEGER NOT NULL,
    UNIQUE (host_id, volume_serial)
);

CREATE TABLE sources (
    source_id              INTEGER PRIMARY KEY,
    host_id                INTEGER NOT NULL,
    name                   TEXT NOT NULL UNIQUE,
    kind                   TEXT NOT NULL,
    root_path              TEXT NOT NULL,
    aliases                TEXT NOT NULL DEFAULT '[]',
    state                  TEXT NOT NULL DEFAULT 'new',
    state_reason           TEXT,
    policy_version         INTEGER NOT NULL DEFAULT 1,
    root_object_id         INTEGER,
    published_generation   INTEGER,
    volume_id              INTEGER,
    preserve_offline       INTEGER NOT NULL DEFAULT 1,
    reconcile_interval_s   INTEGER,
    checkpoint_kind        TEXT,
    checkpoint_json        TEXT,
    checkpoint_at          INTEGER,
    last_scan_started_at   INTEGER,
    last_scan_completed_at INTEGER,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);

CREATE TABLE scan_generations (
    source_id    INTEGER NOT NULL,
    generation   INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    state        TEXT NOT NULL,
    started_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    published_at INTEGER,
    dirs_listed  INTEGER NOT NULL DEFAULT 0,
    entries_seen INTEGER NOT NULL DEFAULT 0,
    errors       INTEGER NOT NULL DEFAULT 0,
    tombstoned   INTEGER NOT NULL DEFAULT 0,
    note         TEXT,
    PRIMARY KEY (source_id, generation)
);

CREATE TABLE objects (
    object_id             INTEGER PRIMARY KEY,
    source_id             INTEGER NOT NULL,
    kind                  TEXT NOT NULL,
    native_volume_serial  INTEGER,
    native_id_high        INTEGER,
    native_id_low         INTEGER,
    identity_confidence   TEXT NOT NULL,
    generation            INTEGER NOT NULL DEFAULT 1,
    size                  INTEGER NOT NULL DEFAULT 0,
    allocated             INTEGER NOT NULL DEFAULT 0,
    attributes            INTEGER NOT NULL DEFAULT 0,
    created               INTEGER,
    modified              INTEGER,
    changed               INTEGER,
    accessed              INTEGER,
    reparse_tag           INTEGER NOT NULL DEFAULT 0,
    link_count            INTEGER NOT NULL DEFAULT 1,
    content_state         TEXT NOT NULL DEFAULT 'pending',
    content_id            BLOB,
    listed_generation     INTEGER,
    first_seen_generation INTEGER NOT NULL,
    last_seen_generation  INTEGER NOT NULL,
    deleted_at            INTEGER
);
CREATE UNIQUE INDEX objects_native
    ON objects (source_id, native_volume_serial, native_id_high, native_id_low)
    WHERE native_id_low IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX objects_source_seen ON objects (source_id, last_seen_generation) WHERE deleted_at IS NULL;
CREATE INDEX objects_content_state ON objects (source_id, content_state) WHERE deleted_at IS NULL;

CREATE TABLE entries (
    entry_id              INTEGER PRIMARY KEY,
    source_id             INTEGER NOT NULL,
    parent_id             INTEGER,
    object_id             INTEGER NOT NULL,
    name                  TEXT NOT NULL,
    name_folded           TEXT NOT NULL,
    extension             TEXT NOT NULL DEFAULT '',
    is_virtual            INTEGER NOT NULL DEFAULT 0,
    first_seen_generation INTEGER NOT NULL,
    last_seen_generation  INTEGER NOT NULL,
    deleted_at            INTEGER
);
CREATE UNIQUE INDEX entries_parent_name
    ON entries (source_id, parent_id, name) WHERE deleted_at IS NULL;
CREATE INDEX entries_object ON entries (object_id);
CREATE INDEX entries_parent_folded ON entries (parent_id, name_folded) WHERE deleted_at IS NULL;
CREATE INDEX entries_source_seen ON entries (source_id, last_seen_generation) WHERE deleted_at IS NULL;
CREATE INDEX entries_extension ON entries (source_id, extension) WHERE deleted_at IS NULL;

CREATE TABLE directory_aggregates (
    object_id        INTEGER PRIMARY KEY,
    source_id        INTEGER NOT NULL,
    file_count       INTEGER NOT NULL DEFAULT 0,
    dir_count        INTEGER NOT NULL DEFAULT 0,
    logical_bytes    INTEGER NOT NULL DEFAULT 0,
    allocated_bytes  INTEGER NOT NULL DEFAULT 0,
    newest_modified  INTEGER,
    oldest_modified  INTEGER,
    content_pending  INTEGER NOT NULL DEFAULT 0,
    content_indexed  INTEGER NOT NULL DEFAULT 0,
    content_failed   INTEGER NOT NULL DEFAULT 0,
    content_excluded INTEGER NOT NULL DEFAULT 0,
    generation       INTEGER NOT NULL,
    complete         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX directory_aggregates_source_bytes ON directory_aggregates (source_id, logical_bytes);

CREATE TABLE directory_extension_counts (
    object_id  INTEGER NOT NULL,
    extension  TEXT NOT NULL,
    count      INTEGER NOT NULL,
    bytes      INTEGER NOT NULL,
    PRIMARY KEY (object_id, extension)
) WITHOUT ROWID;
CREATE INDEX directory_extension_counts_ext ON directory_extension_counts (extension, count);

CREATE TABLE policy_decisions (
    object_id      INTEGER NOT NULL,
    stage          TEXT NOT NULL,
    included       INTEGER NOT NULL,
    reason         TEXT NOT NULL,
    rule           TEXT NOT NULL DEFAULT '',
    policy_version INTEGER NOT NULL,
    user_override  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (object_id, stage)
) WITHOUT ROWID;
CREATE INDEX policy_decisions_reason ON policy_decisions (stage, reason);

CREATE TABLE errors (
    error_id    INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL,
    object_id   INTEGER,
    generation  INTEGER,
    stage       TEXT NOT NULL,
    kind        TEXT NOT NULL,
    code        INTEGER NOT NULL DEFAULT 0,
    path        TEXT NOT NULL DEFAULT '',
    message     TEXT NOT NULL,
    occurred_at INTEGER NOT NULL,
    resolved_at INTEGER
);
CREATE INDEX errors_source_open ON errors (source_id, occurred_at) WHERE resolved_at IS NULL;
"#,
    ),
    (
        "durable jobs and outbox",
        r#"
CREATE TABLE jobs (
    job_id            INTEGER PRIMARY KEY,
    source_id         INTEGER NOT NULL,
    object_id         INTEGER,
    object_generation INTEGER NOT NULL DEFAULT 0,
    stage             TEXT NOT NULL,
    priority          INTEGER NOT NULL,
    state             TEXT NOT NULL DEFAULT 'queued',
    attempts          INTEGER NOT NULL DEFAULT 0,
    idempotency_key   TEXT NOT NULL UNIQUE,
    payload           TEXT,
    estimated_cost    INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    scheduled_at      INTEGER NOT NULL,
    started_at        INTEGER,
    finished_at       INTEGER,
    worker            TEXT,
    last_error        TEXT,
    failure_class     TEXT
);
CREATE INDEX jobs_queue ON jobs (state, stage, priority, scheduled_at);
CREATE INDEX jobs_object ON jobs (object_id, stage);
CREATE INDEX jobs_source_state ON jobs (source_id, state);

CREATE TABLE outbox (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id   INTEGER NOT NULL,
    object_id   INTEGER NOT NULL,
    op          TEXT NOT NULL,
    generation  INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    consumed_at INTEGER
);
CREATE INDEX outbox_unconsumed ON outbox (seq) WHERE consumed_at IS NULL;
"#,
    ),
    (
        "projection state",
        r#"
CREATE TABLE projection_state (
    name        TEXT PRIMARY KEY,
    outbox_seq  INTEGER NOT NULL DEFAULT 0,
    detail      TEXT NOT NULL DEFAULT '{}',
    updated_at  INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE projection_sources (
    name        TEXT NOT NULL,
    source_id   INTEGER NOT NULL,
    generation  INTEGER NOT NULL,
    documents   INTEGER NOT NULL DEFAULT 0,
    built_at    INTEGER NOT NULL,
    PRIMARY KEY (name, source_id)
) WITHOUT ROWID;
"#,
    ),
    (
        "content records, chunks, and per-source content policy",
        r#"
CREATE TABLE content_records (
    object_id          INTEGER PRIMARY KEY,
    source_id          INTEGER NOT NULL,
    generation         INTEGER NOT NULL,
    extraction_version INTEGER NOT NULL,
    encoding           TEXT,
    coverage           TEXT NOT NULL,
    indexed_bytes      INTEGER NOT NULL DEFAULT 0,
    total_bytes        INTEGER NOT NULL DEFAULT 0,
    chunk_count        INTEGER NOT NULL DEFAULT 0,
    line_count         INTEGER NOT NULL DEFAULT 0,
    chars              INTEGER NOT NULL DEFAULT 0,
    content_id         BLOB,
    hash_complete      INTEGER NOT NULL DEFAULT 0,
    state              TEXT NOT NULL,
    failure_class      TEXT,
    error              TEXT,
    reason             TEXT,
    processed_at       INTEGER NOT NULL,
    elapsed_ms         REAL NOT NULL DEFAULT 0
);
CREATE INDEX content_records_source_state ON content_records (source_id, state);
CREATE INDEX content_records_content_id ON content_records (content_id) WHERE content_id IS NOT NULL;

CREATE TABLE chunks (
    object_id  INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    ordinal    INTEGER NOT NULL,
    byte_start INTEGER NOT NULL,
    byte_end   INTEGER NOT NULL,
    line_start INTEGER NOT NULL,
    line_end   INTEGER NOT NULL,
    chars      INTEGER NOT NULL,
    text       BLOB NOT NULL,
    PRIMARY KEY (object_id, generation, ordinal)
) WITHOUT ROWID;

ALTER TABLE sources ADD COLUMN content_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE sources ADD COLUMN content_concurrency INTEGER NOT NULL DEFAULT 2;
"#,
    ),
    (
        "archive manifests: records and virtual members (ADR-0010)",
        r#"
CREATE TABLE archive_records (
    object_id          INTEGER PRIMARY KEY,
    source_id          INTEGER NOT NULL,
    generation         INTEGER NOT NULL,
    format             TEXT NOT NULL,
    member_count       INTEGER NOT NULL DEFAULT 0,
    dir_count          INTEGER NOT NULL DEFAULT 0,
    implicit_dir_count INTEGER NOT NULL DEFAULT 0,
    suspicious_count   INTEGER NOT NULL DEFAULT 0,
    declared_size      INTEGER NOT NULL DEFAULT 0,
    compressed_size    INTEGER NOT NULL DEFAULT 0,
    claimed_entries    INTEGER NOT NULL DEFAULT 0,
    zip64              INTEGER NOT NULL DEFAULT 0,
    truncated          INTEGER NOT NULL DEFAULT 0,
    comment            TEXT,
    state              TEXT NOT NULL,
    error              TEXT,
    reason             TEXT,
    processed_at       INTEGER NOT NULL,
    elapsed_ms         REAL NOT NULL DEFAULT 0
);
CREATE INDEX archive_records_source_state ON archive_records (source_id, state);

CREATE TABLE archive_members (
    object_id  INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    ordinal    INTEGER NOT NULL,
    path       TEXT NOT NULL,
    name       TEXT NOT NULL,
    parent     TEXT NOT NULL,
    raw_name   TEXT NOT NULL,
    is_dir     INTEGER NOT NULL,
    implicit   INTEGER NOT NULL,
    size       INTEGER NOT NULL,
    compressed INTEGER NOT NULL,
    method     INTEGER NOT NULL,
    crc32      INTEGER NOT NULL,
    modified   INTEGER,
    encrypted  INTEGER NOT NULL,
    flags      INTEGER NOT NULL,
    PRIMARY KEY (object_id, generation, ordinal)
) WITHOUT ROWID;
CREATE INDEX archive_members_parent ON archive_members (object_id, generation, parent, name);
"#,
    ),
    (
        "materialized archive members",
        r#"
ALTER TABLE objects ADD COLUMN archive_container_id INTEGER;
ALTER TABLE objects ADD COLUMN archive_generation INTEGER;
ALTER TABLE objects ADD COLUMN archive_member_ordinal INTEGER;

CREATE UNIQUE INDEX objects_archive_member
    ON objects (archive_container_id, archive_generation, archive_member_ordinal)
    WHERE archive_container_id IS NOT NULL;
CREATE INDEX objects_archive_container
    ON objects (archive_container_id, archive_generation);

DROP INDEX entries_parent_name;
CREATE UNIQUE INDEX entries_parent_name
    ON entries (source_id, parent_id, name)
    WHERE deleted_at IS NULL AND is_virtual = 0;
CREATE UNIQUE INDEX entries_virtual_object
    ON entries (object_id) WHERE is_virtual = 1;
"#,
    ),
    (
        "operator retry bookkeeping for jobs",
        r#"
ALTER TABLE jobs ADD COLUMN requeued_at INTEGER;
ALTER TABLE jobs ADD COLUMN requeue_count INTEGER NOT NULL DEFAULT 0;
-- `attempts` at the last operator requeue: history is preserved while the
-- automatic transient budget starts again from that baseline.
ALTER TABLE jobs ADD COLUMN retry_base_attempts INTEGER NOT NULL DEFAULT 0;

CREATE INDEX jobs_failed ON jobs (source_id, stage, failure_class) WHERE state = 'failed';
"#,
    ),
    (
        "interaction events (data collection only)",
        r#"
-- What a search presented and what the person did next. Never the query
-- text: `query_hash` is a stable digest of the normalized query and
-- `query_shape` a coarse label, so a session can be studied without the
-- catalog becoming a log of what anyone searched for.
CREATE TABLE interaction_events (
    id             INTEGER PRIMARY KEY,
    ts             INTEGER NOT NULL,
    query_hash     TEXT NOT NULL,
    query_shape    TEXT NOT NULL,
    object_id      INTEGER,
    source_id      INTEGER,
    presented_rank INTEGER,
    action         TEXT NOT NULL,
    session_id     TEXT NOT NULL
);
-- Retention prunes by age and the row cap deletes the oldest ids; both walk
-- this index instead of the table.
CREATE INDEX interaction_events_ts ON interaction_events (ts);
"#,
    ),
];

/// Apply pending migrations. Returns the versions applied.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<Vec<i64>> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let mut applied = Vec::new();
    for (idx, (desc, sql)) in MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, description, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![version, desc, eidos_domain::UnixNanos::now().0],
        )?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, desc, "applied catalog migration");
        applied.push(version);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        let first = migrate(&mut conn).unwrap();
        assert_eq!(first, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let second = migrate(&mut conn).unwrap();
        assert!(second.is_empty());
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, CURRENT_VERSION);
    }
}
