use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Result, StoreError};

/// Latest metadata schema understood by this build.
pub const LATEST_SCHEMA_VERSION: u32 = 2;

struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
CREATE TABLE schema_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version >= 1),
    updated_unix_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE objects (
    id TEXT PRIMARY KEY CHECK (length(id) = 64),
    logical_size INTEGER NOT NULL CHECK (logical_size >= 0),
    stored_size INTEGER NOT NULL CHECK (stored_size >= 0),
    compression TEXT NOT NULL CHECK (compression IN ('none')),
    created_unix_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE snapshots (
    id TEXT PRIMARY KEY CHECK (length(id) = 64),
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
    entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0),
    created_unix_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE snapshot_entries (
    snapshot_id TEXT NOT NULL REFERENCES snapshots(id) ON DELETE RESTRICT,
    path TEXT NOT NULL CHECK (path <> ''),
    kind TEXT NOT NULL CHECK (kind IN ('file', 'directory', 'symlink')),
    object_id TEXT REFERENCES objects(id) ON DELETE RESTRICT,
    symlink_target TEXT,
    executable INTEGER NOT NULL CHECK (executable IN (0, 1)),
    unix_mode INTEGER NOT NULL CHECK (unix_mode >= 0 AND unix_mode <= 4095),
    logical_size INTEGER NOT NULL CHECK (logical_size >= 0),
    PRIMARY KEY (snapshot_id, path),
    CHECK (
        (kind = 'file' AND object_id IS NOT NULL AND symlink_target IS NULL) OR
        (kind = 'directory' AND object_id IS NULL AND symlink_target IS NULL AND logical_size = 0) OR
        (kind = 'symlink' AND object_id IS NULL AND symlink_target IS NOT NULL AND logical_size = 0)
    )
) WITHOUT ROWID, STRICT;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    command TEXT NOT NULL CHECK (command <> ''),
    workspace_root TEXT NOT NULL,
    started_unix_ms INTEGER NOT NULL,
    finished_unix_ms INTEGER,
    monotonic_duration_ns INTEGER CHECK (monotonic_duration_ns IS NULL OR monotonic_duration_ns >= 0),
    status TEXT NOT NULL CHECK (status IN ('preparing', 'running', 'completed', 'failed', 'interrupted', 'crashed')),
    platform TEXT NOT NULL CHECK (platform IN ('macos_aarch64', 'linux_x86_64', 'linux_aarch64')),
    record_input TEXT NOT NULL CHECK (record_input IN ('auto', 'always', 'never')),
    capture_environment INTEGER NOT NULL CHECK (capture_environment IN (0, 1)),
    initial_snapshot_id TEXT REFERENCES snapshots(id) ON DELETE RESTRICT,
    final_snapshot_id TEXT REFERENCES snapshots(id) ON DELETE RESTRICT,
    exit_kind TEXT CHECK (exit_kind IS NULL OR exit_kind IN ('code', 'signal', 'unknown')),
    exit_value INTEGER,
    CHECK ((status IN ('preparing', 'running')) = (finished_unix_ms IS NULL)),
    CHECK ((status IN ('preparing', 'running')) = (monotonic_duration_ns IS NULL)),
    CHECK (
        (exit_kind IS NULL AND exit_value IS NULL) OR
        (exit_kind IN ('code', 'signal') AND exit_value IS NOT NULL) OR
        (exit_kind = 'unknown' AND exit_value IS NULL)
    )
) STRICT;

CREATE TABLE run_arguments (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    value TEXT NOT NULL,
    PRIMARY KEY (run_id, position)
) WITHOUT ROWID, STRICT;

CREATE TABLE run_parents (
    child_run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    parent_run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    checkpoint_id TEXT NOT NULL,
    CHECK (child_run_id <> parent_run_id),
    FOREIGN KEY (parent_run_id, checkpoint_id)
        REFERENCES checkpoints(run_id, id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE events (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    id TEXT NOT NULL UNIQUE,
    wall_unix_ms INTEGER NOT NULL,
    monotonic_offset_ns INTEGER NOT NULL CHECK (monotonic_offset_ns >= 0),
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    PRIMARY KEY (run_id, sequence)
) WITHOUT ROWID, STRICT;

CREATE TABLE checkpoints (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    label TEXT,
    reason TEXT NOT NULL CHECK (reason IN ('initial', 'manual', 'process_boundary', 'filesystem_quiescence', 'agent_adapter', 'final')),
    snapshot_id TEXT NOT NULL REFERENCES snapshots(id) ON DELETE RESTRICT,
    created_unix_ms INTEGER NOT NULL,
    monotonic_offset_ns INTEGER NOT NULL CHECK (monotonic_offset_ns >= 0),
    UNIQUE (run_id, id),
    UNIQUE (run_id, sequence)
) STRICT;

CREATE TABLE terminal_chunks (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    stream_id TEXT NOT NULL,
    first_sequence INTEGER NOT NULL CHECK (first_sequence >= 0),
    last_sequence INTEGER NOT NULL CHECK (last_sequence >= first_sequence),
    object_id TEXT NOT NULL REFERENCES objects(id) ON DELETE RESTRICT,
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    PRIMARY KEY (run_id, stream_id, first_sequence)
) WITHOUT ROWID, STRICT;

CREATE TABLE warnings (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (run_id, sequence)
) WITHOUT ROWID, STRICT;
"#,
    },
    Migration {
        version: 2,
        sql: r#"
CREATE INDEX events_wall_time ON events(run_id, wall_unix_ms, sequence);
CREATE INDEX checkpoints_snapshot ON checkpoints(snapshot_id);
CREATE INDEX runs_started ON runs(started_unix_ms DESC, id DESC);
CREATE INDEX run_parents_parent ON run_parents(parent_run_id);
CREATE INDEX snapshot_entries_object ON snapshot_entries(object_id) WHERE object_id IS NOT NULL;
CREATE INDEX terminal_chunks_sequence ON terminal_chunks(run_id, first_sequence, last_sequence);
"#,
    },
];

pub(crate) fn migrate(connection: &mut Connection, now_unix_ms: i64) -> Result<()> {
    let current: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| StoreError::Database {
            operation: "read schema version",
            source,
        })?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::NewerSchema {
            found: current,
            supported: LATEST_SCHEMA_VERSION,
        });
    }

    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin schema migration",
                source,
            })?;
        transaction
            .execute_batch(migration.sql)
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .execute(
                "INSERT INTO schema_metadata(singleton, version, updated_unix_ms) VALUES (1, ?1, ?2)\n                 ON CONFLICT(singleton) DO UPDATE SET version = excluded.version, updated_unix_ms = excluded.updated_unix_ms",
                params![migration.version, now_unix_ms],
            )
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .pragma_update(None, "user_version", migration.version)
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .commit()
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
    }

    verify(connection)
}

pub(crate) fn verify(connection: &Connection) -> Result<()> {
    let pragma: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| StoreError::Database {
            operation: "verify schema version",
            source,
        })?;
    let metadata: u32 = connection
        .query_row(
            "SELECT version FROM schema_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|source| StoreError::Database {
            operation: "verify schema metadata",
            source,
        })?;
    if pragma != LATEST_SCHEMA_VERSION || metadata != LATEST_SCHEMA_VERSION {
        return Err(StoreError::SchemaMismatch { pragma, metadata });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn migrate_only_through(
    connection: &mut Connection,
    through: u32,
    now_unix_ms: i64,
) -> Result<()> {
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= through)
    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin test schema migration",
                source,
            })?;
        transaction
            .execute_batch(migration.sql)
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .execute(
                "INSERT INTO schema_metadata(singleton, version, updated_unix_ms) VALUES (1, ?1, ?2)\n                 ON CONFLICT(singleton) DO UPDATE SET version = excluded.version, updated_unix_ms = excluded.updated_unix_ms",
                params![migration.version, now_unix_ms],
            )
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .pragma_update(None, "user_version", migration.version)
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
        transaction
            .commit()
            .map_err(|source| StoreError::Migration {
                version: migration.version,
                source,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_database_reaches_latest_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate(&mut connection, 1).unwrap();
        verify(&connection).unwrap();

        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name IN ('runs', 'events', 'checkpoints', 'snapshots', 'objects')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 5);
    }

    #[test]
    fn version_one_upgrades_without_losing_rows() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate_only_through(&mut connection, 1, 1).unwrap();
        connection
            .execute(
                "INSERT INTO objects VALUES (?1, 3, 19, 'none', 1)",
                ["0".repeat(64)],
            )
            .unwrap();

        migrate(&mut connection, 2).unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        verify(&connection).unwrap();
    }

    #[test]
    fn failed_migration_rolls_back_version_and_preserves_data() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate_only_through(&mut connection, 1, 1).unwrap();
        connection
            .execute(
                "INSERT INTO objects VALUES (?1, 3, 19, 'none', 1)",
                ["0".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "CREATE INDEX events_wall_time ON objects(created_unix_ms)",
                [],
            )
            .unwrap();

        assert!(matches!(
            migrate(&mut connection, 2),
            Err(StoreError::Migration { version: 2, .. })
        ));
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(count, 1);
    }
}
