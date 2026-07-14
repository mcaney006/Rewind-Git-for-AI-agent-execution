use rewind_domain::{Run, RunId, Timestamp};
use rusqlite::{TransactionBehavior, params};

use crate::run::{decode_u64, parse_id};
use crate::{Result, Store, StoreError};

/// A safe diagnostic stored separately from the authoritative event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarningRecord {
    /// Owning run.
    pub run_id: RunId,
    /// Warning order within the run.
    pub sequence: u64,
    /// Stable machine-readable category.
    pub code: String,
    /// Safe human-readable diagnostic.
    pub message: String,
    /// Wall-clock creation time.
    pub created_at: Timestamp,
}

/// Store-derived evidence needed by filesystem comparison orchestration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonInput {
    /// Durable run metadata, including ancestry and outcome.
    pub run: Run,
    /// Number of committed checkpoints.
    pub checkpoint_count: u64,
    /// Total indexed raw terminal-output bytes.
    pub terminal_output_bytes: u64,
    /// Number of store diagnostics.
    pub warning_count: u64,
}

impl Store {
    /// Recovers orphaned active records without inventing terminal events.
    ///
    /// The last recorded monotonic event offset becomes the best available
    /// duration. A separate warning makes that limitation visible.
    pub fn mark_interrupted_runs(&mut self, recovered_at: Timestamp) -> Result<Vec<RunId>> {
        self.require_writer("recover interrupted runs")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin interrupted-run recovery",
                source,
            })?;
        let raw_ids = {
            let mut statement = transaction
                .prepare("SELECT id FROM runs WHERE status IN ('preparing', 'running') ORDER BY id")
                .map_err(|source| StoreError::Database {
                    operation: "prepare interrupted-run recovery",
                    source,
                })?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|source| StoreError::Database {
                    operation: "query interrupted runs",
                    source,
                })?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row.map_err(|source| StoreError::Database {
                    operation: "read interrupted run ID",
                    source,
                })?);
            }
            ids
        };
        let mut run_ids = Vec::with_capacity(raw_ids.len());
        for raw_id in raw_ids {
            let run_id = parse_id::<RunId>("run ID", &raw_id)?;
            transaction
                .execute(
                    "UPDATE runs SET\n                        status = 'interrupted',\n                        finished_unix_ms = ?2,\n                        monotonic_duration_ns = COALESCE(\n                            (SELECT MAX(monotonic_offset_ns) FROM events WHERE events.run_id = runs.id),\n                            0\n                        )\n                     WHERE id = ?1 AND status IN ('preparing', 'running')",
                    params![run_id.to_string(), recovered_at.as_unix_milliseconds()],
                )
                .map_err(|source| StoreError::Database {
                    operation: "mark interrupted run",
                    source,
                })?;
            transaction
                .execute(
                    "INSERT INTO warnings(run_id, sequence, code, message, created_unix_ms)\n                     VALUES (\n                        ?1,\n                        COALESCE((SELECT MAX(sequence) + 1 FROM warnings WHERE run_id = ?1), 1),\n                        'incomplete_run_recovered',\n                        'A previous recorder stopped without finalizing this run.',\n                        ?2\n                     )",
                    params![run_id.to_string(), recovered_at.as_unix_milliseconds()],
                )
                .map_err(|source| StoreError::Database {
                    operation: "record interrupted-run warning",
                    source,
                })?;
            run_ids.push(run_id);
        }
        transaction
            .commit()
            .map_err(|source| StoreError::Database {
                operation: "commit interrupted-run recovery",
                source,
            })?;
        Ok(run_ids)
    }

    /// Loads safe store diagnostics in their recorded order.
    pub fn load_warnings(&self, run_id: RunId) -> Result<Vec<WarningRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, code, message, created_unix_ms FROM warnings\n                 WHERE run_id = ?1 ORDER BY sequence",
            )
            .map_err(|source| StoreError::Database {
                operation: "prepare run warnings",
                source,
            })?;
        let rows = statement
            .query_map([run_id.to_string()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|source| StoreError::Database {
                operation: "query run warnings",
                source,
            })?;
        let mut warnings = Vec::new();
        for row in rows {
            let (sequence, code, message, created_unix_ms) =
                row.map_err(|source| StoreError::Database {
                    operation: "read run warning",
                    source,
                })?;
            warnings.push(WarningRecord {
                run_id,
                sequence: decode_u64("warning sequence", sequence)?,
                code,
                message,
                created_at: Timestamp::from_unix_milliseconds(created_unix_ms),
            });
        }
        Ok(warnings)
    }

    /// Loads metadata evidence without deciding which outcome is better.
    pub fn load_comparison_input(&self, run_id: RunId) -> Result<ComparisonInput> {
        let run = self.load_run(run_id)?;
        let (checkpoint_count, terminal_output_bytes, warning_count): (i64, i64, i64) = self
            .connection
            .query_row(
                "SELECT\n                    (SELECT COUNT(*) FROM checkpoints WHERE run_id = ?1),\n                    (SELECT COALESCE(SUM(byte_length), 0) FROM terminal_chunks WHERE run_id = ?1),\n                    (SELECT COUNT(*) FROM warnings WHERE run_id = ?1)",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|source| StoreError::Database {
                operation: "load comparison evidence",
                source,
            })?;
        Ok(ComparisonInput {
            run,
            checkpoint_count: decode_u64("checkpoint count", checkpoint_count)?,
            terminal_output_bytes: decode_u64("terminal output bytes", terminal_output_bytes)?,
            warning_count: decode_u64("warning count", warning_count)?,
        })
    }
}
