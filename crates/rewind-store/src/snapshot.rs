use std::str::FromStr;

use rewind_domain::{
    Checkpoint, CheckpointId, CheckpointReason, Event, EventPayload, EventSequence,
    MonotonicDuration, ObjectId, RunId, Snapshot, SnapshotEntry, SnapshotEntryKind, SnapshotId,
    SnapshotManifest, SnapshotPath, Timestamp, UnixPermissions,
};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::event::append_event_batch_in;
use crate::run::{decode_u64, parse_enum, parse_id};
use crate::{Result, Store, StoreError, sql_u64};

#[derive(Debug)]
struct RawEntry {
    path: String,
    kind: String,
    object_id: Option<String>,
    symlink_target: Option<String>,
    executable: bool,
    unix_mode: i64,
    logical_size: i64,
}

impl Store {
    /// Persists a canonical snapshot manifest after verifying its identity and object references.
    pub fn store_snapshot(&mut self, snapshot: &Snapshot, created_at: Timestamp) -> Result<()> {
        self.require_writer("store a snapshot")?;
        validate_snapshot(snapshot)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin snapshot storage",
                source,
            })?;
        store_snapshot_in(&transaction, snapshot, created_at)?;
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit snapshot storage",
            source,
        })
    }

    /// Commits a snapshot and checkpoint together, so metadata never references a partial tree.
    pub fn commit_checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
        snapshot: &Snapshot,
    ) -> Result<()> {
        self.require_writer("commit a checkpoint")?;
        validate_checkpoint_snapshot(checkpoint, snapshot)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin checkpoint commit",
                source,
            })?;
        commit_checkpoint_in(&transaction, checkpoint, snapshot)?;
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit checkpoint transaction",
            source,
        })
    }

    /// Atomically commits a checkpoint and its matching timeline event.
    pub fn commit_checkpoint_with_event(
        &mut self,
        checkpoint: &Checkpoint,
        snapshot: &Snapshot,
        event: &Event,
    ) -> Result<()> {
        self.require_writer("commit a checkpoint event")?;
        validate_checkpoint_snapshot(checkpoint, snapshot)?;
        let matches = event.run_id == checkpoint.run_id
            && event.sequence == checkpoint.sequence
            && event.wall_clock == checkpoint.created_at
            && event.monotonic_offset == checkpoint.monotonic_offset
            && matches!(
                &event.payload,
                EventPayload::CheckpointCommitted {
                    checkpoint_id,
                    snapshot_id
                } if *checkpoint_id == checkpoint.id && *snapshot_id == checkpoint.snapshot_id
            );
        if !matches {
            return Err(StoreError::InvalidCheckpoint {
                id: checkpoint.id.to_string(),
                message: "committed event does not exactly describe the checkpoint".to_owned(),
            });
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin checkpoint event commit",
                source,
            })?;
        commit_checkpoint_in(&transaction, checkpoint, snapshot)?;
        append_event_batch_in(&transaction, std::slice::from_ref(event))?;
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit checkpoint and event transaction",
            source,
        })
    }

    /// Loads and revalidates a complete canonical snapshot manifest.
    pub fn load_snapshot(&self, snapshot_id: SnapshotId) -> Result<Snapshot> {
        load_snapshot_from(&self.connection, snapshot_id)
    }

    /// Loads one committed checkpoint.
    pub fn load_checkpoint(&self, checkpoint_id: CheckpointId) -> Result<Checkpoint> {
        let raw = self
            .connection
            .query_row(
                "SELECT id, run_id, sequence, label, reason, snapshot_id, created_unix_ms, monotonic_offset_ns\n                 FROM checkpoints WHERE id = ?1",
                [checkpoint_id.to_string()],
                raw_checkpoint,
            )
            .optional()
            .map_err(|source| StoreError::Database {
                operation: "load checkpoint",
                source,
            })?
            .ok_or_else(|| StoreError::NotFound {
                entity: "checkpoint",
                id: checkpoint_id.to_string(),
            })?;
        decode_checkpoint(raw)
    }

    /// Loads a run's checkpoints in authoritative event-sequence order.
    pub fn load_checkpoints(&self, run_id: RunId) -> Result<Vec<Checkpoint>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, run_id, sequence, label, reason, snapshot_id, created_unix_ms, monotonic_offset_ns\n                 FROM checkpoints WHERE run_id = ?1 ORDER BY sequence",
            )
            .map_err(|source| StoreError::Database {
                operation: "prepare checkpoint timeline",
                source,
            })?;
        let rows = statement
            .query_map([run_id.to_string()], raw_checkpoint)
            .map_err(|source| StoreError::Database {
                operation: "query checkpoint timeline",
                source,
            })?;
        let mut checkpoints = Vec::new();
        for row in rows {
            checkpoints.push(decode_checkpoint(row.map_err(|source| {
                StoreError::Database {
                    operation: "read checkpoint timeline",
                    source,
                }
            })?)?);
        }
        Ok(checkpoints)
    }
}

fn validate_checkpoint_snapshot(checkpoint: &Checkpoint, snapshot: &Snapshot) -> Result<()> {
    checkpoint
        .validate()
        .map_err(|source| StoreError::InvalidCheckpoint {
            id: checkpoint.id.to_string(),
            message: source.to_string(),
        })?;
    if checkpoint.snapshot_id != snapshot.id {
        return Err(StoreError::InvalidCheckpoint {
            id: checkpoint.id.to_string(),
            message: format!(
                "checkpoint snapshot {} does not match supplied snapshot {}",
                checkpoint.snapshot_id, snapshot.id
            ),
        });
    }
    validate_snapshot(snapshot)
}

fn commit_checkpoint_in(
    transaction: &Transaction<'_>,
    checkpoint: &Checkpoint,
    snapshot: &Snapshot,
) -> Result<()> {
    store_snapshot_in(transaction, snapshot, checkpoint.created_at)?;
    transaction
        .execute(
            "INSERT INTO checkpoints(\n                id, run_id, sequence, label, reason, snapshot_id, created_unix_ms, monotonic_offset_ns\n             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                checkpoint.id.to_string(),
                checkpoint.run_id.to_string(),
                sql_u64("checkpoint sequence", checkpoint.sequence.get())?,
                checkpoint.label,
                checkpoint.reason.as_str(),
                checkpoint.snapshot_id.to_string(),
                checkpoint.created_at.as_unix_milliseconds(),
                sql_u64(
                    "checkpoint monotonic offset",
                    checkpoint.monotonic_offset.as_nanoseconds()
                )?,
            ],
        )
        .map_err(|source| StoreError::Database {
            operation: "insert checkpoint",
            source,
        })?;
    match checkpoint.reason {
        CheckpointReason::Initial => set_run_snapshot(
            transaction,
            checkpoint.run_id,
            "initial_snapshot_id",
            checkpoint.snapshot_id,
        )?,
        CheckpointReason::Final => set_run_snapshot(
            transaction,
            checkpoint.run_id,
            "final_snapshot_id",
            checkpoint.snapshot_id,
        )?,
        CheckpointReason::Manual
        | CheckpointReason::ProcessBoundary
        | CheckpointReason::FilesystemQuiescence
        | CheckpointReason::AgentAdapter => {}
    }
    Ok(())
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    let canonical =
        serde_json::to_vec(&snapshot.manifest).map_err(|source| StoreError::Serialization {
            context: "snapshot manifest",
            source,
        })?;
    let actual = SnapshotId::digest(&canonical);
    if actual != snapshot.id {
        return Err(StoreError::InvalidSnapshot {
            id: snapshot.id.to_string(),
            message: format!("canonical manifest digest is {actual}"),
        });
    }
    Ok(())
}

fn store_snapshot_in(
    transaction: &Transaction<'_>,
    snapshot: &Snapshot,
    created_at: Timestamp,
) -> Result<()> {
    let entry_count =
        i64::try_from(snapshot.manifest.entries().len()).map_err(|_| StoreError::NumericRange {
            field: "snapshot entry count",
            value: snapshot.manifest.entries().len() as u128,
        })?;
    let logical_bytes = snapshot
        .manifest
        .entries()
        .iter()
        .try_fold(0_u64, |total, entry| match &entry.kind {
            SnapshotEntryKind::File { size, .. } => total.checked_add(*size),
            SnapshotEntryKind::Directory | SnapshotEntryKind::Symlink { .. } => Some(total),
        })
        .ok_or(StoreError::NumericRange {
            field: "snapshot logical bytes",
            value: u128::MAX,
        })?;
    let inserted = transaction
        .execute(
            "INSERT INTO snapshots(id, schema_version, entry_count, logical_bytes, created_unix_ms)\n             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO NOTHING",
            params![
                snapshot.id.to_string(),
                snapshot.manifest.schema_version,
                entry_count,
                sql_u64("snapshot logical bytes", logical_bytes)?,
                created_at.as_unix_milliseconds(),
            ],
        )
        .map_err(|source| StoreError::Database {
            operation: "insert snapshot",
            source,
        })?;
    if inserted == 0 {
        let existing = load_snapshot_from(transaction, snapshot.id)?;
        if existing != *snapshot {
            return Err(StoreError::InvalidSnapshot {
                id: snapshot.id.to_string(),
                message: "existing snapshot metadata differs from supplied manifest".to_owned(),
            });
        }
        return Ok(());
    }

    for entry in snapshot.manifest.entries() {
        let (kind, object_id, symlink_target, executable, logical_size) = match &entry.kind {
            SnapshotEntryKind::Directory => ("directory", None, None, false, 0),
            SnapshotEntryKind::File {
                object_id,
                size,
                executable,
            } => (
                "file",
                Some(object_id.to_string()),
                None,
                *executable,
                sql_u64("snapshot file size", *size)?,
            ),
            SnapshotEntryKind::Symlink { target } => {
                ("symlink", None, Some(target.as_str()), false, 0)
            }
        };
        transaction
            .execute(
                "INSERT INTO snapshot_entries(\n                    snapshot_id, path, kind, object_id, symlink_target, executable, unix_mode, logical_size\n                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    snapshot.id.to_string(),
                    entry.path.as_str(),
                    kind,
                    object_id,
                    symlink_target,
                    executable,
                    entry.permissions.bits(),
                    logical_size,
                ],
            )
            .map_err(|source| StoreError::Database {
                operation: "insert snapshot entry",
                source,
            })?;
    }
    Ok(())
}

fn load_snapshot_from(
    connection: &rusqlite::Connection,
    snapshot_id: SnapshotId,
) -> Result<Snapshot> {
    let metadata: Option<(u16, i64, i64)> = connection
        .query_row(
            "SELECT schema_version, entry_count, logical_bytes FROM snapshots WHERE id = ?1",
            [snapshot_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "load snapshot metadata",
            source,
        })?;
    let (schema_version, expected_count, expected_bytes) =
        metadata.ok_or_else(|| StoreError::NotFound {
            entity: "snapshot",
            id: snapshot_id.to_string(),
        })?;
    let mut statement = connection
        .prepare(
            "SELECT path, kind, object_id, symlink_target, executable, unix_mode, logical_size\n             FROM snapshot_entries WHERE snapshot_id = ?1 ORDER BY path",
        )
        .map_err(|source| StoreError::Database {
            operation: "prepare snapshot entries",
            source,
        })?;
    let rows = statement
        .query_map([snapshot_id.to_string()], raw_entry)
        .map_err(|source| StoreError::Database {
            operation: "query snapshot entries",
            source,
        })?;
    let mut entries = Vec::new();
    let mut logical_bytes = 0_u64;
    for row in rows {
        let raw = row.map_err(|source| StoreError::Database {
            operation: "read snapshot entry",
            source,
        })?;
        let permissions_value =
            u16::try_from(raw.unix_mode).map_err(|_| StoreError::InvalidSnapshot {
                id: snapshot_id.to_string(),
                message: format!("invalid Unix mode {} for {}", raw.unix_mode, raw.path),
            })?;
        let permissions = UnixPermissions::new(permissions_value).map_err(|source| {
            StoreError::InvalidSnapshot {
                id: snapshot_id.to_string(),
                message: source.to_string(),
            }
        })?;
        let size = decode_u64("snapshot entry logical size", raw.logical_size)?;
        let kind = match (
            raw.kind.as_str(),
            raw.object_id,
            raw.symlink_target,
            raw.executable,
            size,
        ) {
            ("directory", None, None, false, 0) => SnapshotEntryKind::Directory,
            ("file", Some(object_id), None, executable, size) => {
                logical_bytes =
                    logical_bytes
                        .checked_add(size)
                        .ok_or(StoreError::InvalidSnapshot {
                            id: snapshot_id.to_string(),
                            message: "logical byte total overflowed".to_owned(),
                        })?;
                SnapshotEntryKind::File {
                    object_id: parse_id::<ObjectId>("object ID", &object_id)?,
                    size,
                    executable,
                }
            }
            ("symlink", None, Some(target), false, 0) => SnapshotEntryKind::Symlink { target },
            combination => {
                return Err(StoreError::InvalidSnapshot {
                    id: snapshot_id.to_string(),
                    message: format!("invalid entry columns for {}: {combination:?}", raw.path),
                });
            }
        };
        entries.push(SnapshotEntry {
            path: SnapshotPath::from_str(&raw.path).map_err(|source| {
                StoreError::InvalidSnapshot {
                    id: snapshot_id.to_string(),
                    message: source.to_string(),
                }
            })?,
            kind,
            permissions,
        });
    }
    if i64::try_from(entries.len()).ok() != Some(expected_count)
        || sql_u64("snapshot logical bytes", logical_bytes)? != expected_bytes
    {
        return Err(StoreError::InvalidSnapshot {
            id: snapshot_id.to_string(),
            message: "entry count or logical byte total disagrees with metadata".to_owned(),
        });
    }
    let manifest =
        SnapshotManifest::from_canonical_entries(schema_version, entries).map_err(|source| {
            StoreError::InvalidSnapshot {
                id: snapshot_id.to_string(),
                message: source.to_string(),
            }
        })?;
    let snapshot = Snapshot {
        id: snapshot_id,
        manifest,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn raw_entry(row: &Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        path: row.get(0)?,
        kind: row.get(1)?,
        object_id: row.get(2)?,
        symlink_target: row.get(3)?,
        executable: row.get(4)?,
        unix_mode: row.get(5)?,
        logical_size: row.get(6)?,
    })
}

fn set_run_snapshot(
    transaction: &Transaction<'_>,
    run_id: RunId,
    column: &'static str,
    snapshot_id: SnapshotId,
) -> Result<()> {
    let sql = match column {
        "initial_snapshot_id" => {
            "UPDATE runs SET initial_snapshot_id = ?2 WHERE id = ?1 AND (initial_snapshot_id IS NULL OR initial_snapshot_id = ?2)"
        }
        "final_snapshot_id" => {
            "UPDATE runs SET final_snapshot_id = ?2 WHERE id = ?1 AND (final_snapshot_id IS NULL OR final_snapshot_id = ?2)"
        }
        _ => {
            return Err(StoreError::Invariant {
                message: format!("unsupported run snapshot column {column}"),
            });
        }
    };
    let changed = transaction
        .execute(sql, params![run_id.to_string(), snapshot_id.to_string()])
        .map_err(|source| StoreError::Database {
            operation: "link checkpoint snapshot to run",
            source,
        })?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidCheckpoint {
            id: snapshot_id.to_string(),
            message: format!("run {run_id} is missing or already references a different {column}"),
        })
    }
}

struct RawCheckpoint {
    id: String,
    run_id: String,
    sequence: i64,
    label: Option<String>,
    reason: String,
    snapshot_id: String,
    created_unix_ms: i64,
    monotonic_offset_ns: i64,
}

fn raw_checkpoint(row: &Row<'_>) -> rusqlite::Result<RawCheckpoint> {
    Ok(RawCheckpoint {
        id: row.get(0)?,
        run_id: row.get(1)?,
        sequence: row.get(2)?,
        label: row.get(3)?,
        reason: row.get(4)?,
        snapshot_id: row.get(5)?,
        created_unix_ms: row.get(6)?,
        monotonic_offset_ns: row.get(7)?,
    })
}

fn decode_checkpoint(raw: RawCheckpoint) -> Result<Checkpoint> {
    let sequence_value = decode_u64("checkpoint sequence", raw.sequence)?;
    let sequence =
        EventSequence::new(sequence_value).ok_or_else(|| StoreError::InvalidCheckpoint {
            id: raw.id.clone(),
            message: "checkpoint sequence is zero".to_owned(),
        })?;
    let checkpoint = Checkpoint {
        id: parse_id("checkpoint ID", &raw.id)?,
        run_id: parse_id("checkpoint run ID", &raw.run_id)?,
        sequence,
        label: raw.label,
        reason: parse_enum("checkpoint reason", &raw.reason)?,
        snapshot_id: parse_id("checkpoint snapshot ID", &raw.snapshot_id)?,
        created_at: Timestamp::from_unix_milliseconds(raw.created_unix_ms),
        monotonic_offset: MonotonicDuration::from_nanoseconds(decode_u64(
            "checkpoint monotonic offset",
            raw.monotonic_offset_ns,
        )?),
    };
    checkpoint
        .validate()
        .map_err(|source| StoreError::InvalidCheckpoint {
            id: checkpoint.id.to_string(),
            message: source.to_string(),
        })?;
    Ok(checkpoint)
}
