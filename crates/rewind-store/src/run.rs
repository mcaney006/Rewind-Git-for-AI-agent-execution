use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use rewind_domain::{
    BranchId, CapturePolicy, Event, EventPayload, InputRecordingPolicy, MonotonicDuration,
    ProcessExitStatus, Run, RunId, RunParent, RunStatus, SnapshotId, Timestamp,
};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::event::append_event_batch_in;
use crate::{Result, Store, StoreError, sql_u64};

/// Terminal fields applied exactly once when a run leaves an active state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunFinish {
    /// Legal terminal lifecycle status.
    pub status: RunStatus,
    /// Wall-clock finalization time.
    pub finished_at: Timestamp,
    /// Monotonic elapsed capture time.
    pub monotonic_duration: MonotonicDuration,
    /// Authoritative final workspace snapshot, when available.
    pub final_snapshot: Option<SnapshotId>,
    /// Best observed root-process result.
    pub exit_status: Option<ProcessExitStatus>,
}

#[derive(Debug)]
struct RawRun {
    id: String,
    branch_id: String,
    parent_run_id: Option<String>,
    parent_checkpoint_id: Option<String>,
    command: String,
    workspace_root: String,
    started_unix_ms: i64,
    finished_unix_ms: Option<i64>,
    monotonic_duration_ns: Option<i64>,
    status: String,
    platform: String,
    record_input: String,
    capture_environment: bool,
    initial_snapshot_id: Option<String>,
    final_snapshot_id: Option<String>,
    exit_kind: Option<String>,
    exit_value: Option<i32>,
}

impl Store {
    /// Creates the durable `Preparing` record before any child process starts.
    pub fn create_run(&mut self, run: &Run) -> Result<()> {
        self.require_writer("create a run")?;
        run.validate().map_err(|source| StoreError::InvalidRun {
            id: run.id.to_string(),
            message: source.to_string(),
        })?;
        if run.status != RunStatus::Preparing {
            return Err(StoreError::InvalidRun {
                id: run.id.to_string(),
                message: "new runs must begin in preparing state".to_owned(),
            });
        }
        let workspace_root = run
            .workspace_root
            .to_str()
            .ok_or_else(|| StoreError::InvalidRun {
                id: run.id.to_string(),
                message: "workspace root is not valid UTF-8".to_owned(),
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin run creation",
                source,
            })?;

        if let Some(parent) = run.parent {
            reject_parent_cycle(&transaction, run.id, parent.run_id)?;
        }
        let (exit_kind, exit_value) = encode_exit_status(run.exit_status);
        transaction
            .execute(
                "INSERT INTO runs(\n                    id, branch_id, command, workspace_root, started_unix_ms, finished_unix_ms,\n                    monotonic_duration_ns, status, platform, record_input, capture_environment,\n                    initial_snapshot_id, final_snapshot_id, exit_kind, exit_value\n                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    run.id.to_string(),
                    run.branch_id.to_string(),
                    run.command,
                    workspace_root,
                    run.started_at.as_unix_milliseconds(),
                    run.finished_at.map(Timestamp::as_unix_milliseconds),
                    run.monotonic_duration
                        .map(MonotonicDuration::as_nanoseconds)
                        .map(|value| sql_u64("run monotonic duration", value))
                        .transpose()?,
                    run.status.as_str(),
                    run.platform.as_str(),
                    run.capture_policy.record_input.as_str(),
                    run.capture_policy.capture_environment,
                    run.initial_snapshot.map(|id| id.to_string()),
                    run.final_snapshot.map(|id| id.to_string()),
                    exit_kind,
                    exit_value,
                ],
            )
            .map_err(|source| StoreError::Database {
                operation: "insert run",
                source,
            })?;

        for (position, argument) in run.arguments.iter().enumerate() {
            let position = i64::try_from(position).map_err(|_| StoreError::NumericRange {
                field: "run argument position",
                value: position as u128,
            })?;
            transaction
                .execute(
                    "INSERT INTO run_arguments(run_id, position, value) VALUES (?1, ?2, ?3)",
                    params![run.id.to_string(), position, argument],
                )
                .map_err(|source| StoreError::Database {
                    operation: "insert run argument",
                    source,
                })?;
        }
        if let Some(parent) = run.parent {
            transaction
                .execute(
                    "INSERT INTO run_parents(child_run_id, parent_run_id, checkpoint_id) VALUES (?1, ?2, ?3)",
                    params![
                        run.id.to_string(),
                        parent.run_id.to_string(),
                        parent.checkpoint_id.to_string()
                    ],
                )
                .map_err(|source| StoreError::Database {
                    operation: "insert run parent",
                    source,
                })?;
        }
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit run creation",
            source,
        })
    }

    /// Transitions a prepared run to running after its initial snapshot exists.
    pub fn mark_run_running(&mut self, run_id: RunId, initial_snapshot: SnapshotId) -> Result<()> {
        self.require_writer("mark a run running")?;
        let changed = self
            .connection
            .execute(
                "UPDATE runs SET status = 'running', initial_snapshot_id = ?2\n                 WHERE id = ?1 AND status = 'preparing'",
                params![run_id.to_string(), initial_snapshot.to_string()],
            )
            .map_err(|source| StoreError::Database {
                operation: "mark run running",
                source,
            })?;
        if changed == 1 {
            Ok(())
        } else {
            Err(missing_or_state_error(
                &self.connection,
                run_id,
                "preparing",
            )?)
        }
    }

    /// Applies a legal terminal transition and its final observed metadata atomically.
    pub fn finish_run(&mut self, run_id: RunId, finish: RunFinish) -> Result<()> {
        self.require_writer("finish a run")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin run finalization",
                source,
            })?;
        finish_run_in(&transaction, run_id, finish)?;
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit run finalization",
            source,
        })
    }

    /// Appends the terminal event batch and finalizes its run in one transaction.
    ///
    /// The final event must be `RunCompleted` and agree with the supplied status
    /// and exit result. A crash or constraint failure therefore cannot leave a
    /// terminal timeline attached to an active run row.
    pub fn finish_run_with_events(
        &mut self,
        run_id: RunId,
        finish: RunFinish,
        events: &[Event],
    ) -> Result<()> {
        self.require_writer("finish a run with terminal events")?;
        let last = events.last().ok_or_else(|| StoreError::EventOrder {
            message: format!("run {run_id} finalization requires a terminal event"),
        })?;
        match &last.payload {
            EventPayload::RunCompleted {
                status,
                exit_status,
            } if last.run_id == run_id
                && *status == finish.status
                && *exit_status == finish.exit_status => {}
            EventPayload::RunCompleted { .. } => {
                return Err(StoreError::InvalidEvent {
                    id: last.id.to_string(),
                    message: "terminal event disagrees with run finalization metadata".to_owned(),
                });
            }
            _ => {
                return Err(StoreError::InvalidEvent {
                    id: last.id.to_string(),
                    message: "finalization batch must end with RunCompleted".to_owned(),
                });
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin event and run finalization",
                source,
            })?;
        append_event_batch_in(&transaction, events)?;
        finish_run_in(&transaction, run_id, finish)?;
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit event and run finalization",
            source,
        })
    }

    /// Loads one run and validates its reconstructed domain invariants.
    pub fn load_run(&self, run_id: RunId) -> Result<Run> {
        let raw = self
            .connection
            .query_row(
                &run_select("WHERE runs.id = ?1"),
                [run_id.to_string()],
                raw_run,
            )
            .optional()
            .map_err(|source| StoreError::Database {
                operation: "load run",
                source,
            })?
            .ok_or_else(|| StoreError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            })?;
        let arguments = load_arguments(&self.connection, Some(run_id))?
            .remove(&run_id)
            .unwrap_or_default();
        decode_run(raw, arguments)
    }

    /// Lists runs newest first without issuing one query per run.
    pub fn list_runs(&self) -> Result<Vec<Run>> {
        let mut statement = self
            .connection
            .prepare(&run_select(
                "ORDER BY runs.started_unix_ms DESC, runs.id DESC",
            ))
            .map_err(|source| StoreError::Database {
                operation: "prepare run listing",
                source,
            })?;
        let rows = statement
            .query_map([], raw_run)
            .map_err(|source| StoreError::Database {
                operation: "query run listing",
                source,
            })?;
        let mut raw_runs = Vec::new();
        for row in rows {
            raw_runs.push(row.map_err(|source| StoreError::Database {
                operation: "read run listing",
                source,
            })?);
        }
        drop(statement);
        let mut arguments = load_arguments(&self.connection, None)?;
        raw_runs
            .into_iter()
            .map(|raw| {
                let run_id = parse_id::<RunId>("run ID", &raw.id)?;
                decode_run(raw, arguments.remove(&run_id).unwrap_or_default())
            })
            .collect()
    }
}

fn finish_run_in(transaction: &Transaction<'_>, run_id: RunId, finish: RunFinish) -> Result<()> {
    if !finish.status.is_terminal() {
        return Err(StoreError::InvalidRun {
            id: run_id.to_string(),
            message: format!("finish status {} is not terminal", finish.status),
        });
    }
    let current_text: Option<String> = transaction
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "load run status for finalization",
            source,
        })?;
    let current_text = current_text.ok_or_else(|| StoreError::NotFound {
        entity: "run",
        id: run_id.to_string(),
    })?;
    let current = parse_enum::<RunStatus>("run status", &current_text)?;
    if !current.can_transition_to(finish.status) {
        return Err(StoreError::InvalidRun {
            id: run_id.to_string(),
            message: format!(
                "illegal status transition from {current} to {}",
                finish.status
            ),
        });
    }
    let duration = sql_u64(
        "run monotonic duration",
        finish.monotonic_duration.as_nanoseconds(),
    )?;
    let (exit_kind, exit_value) = encode_exit_status(finish.exit_status);
    transaction
            .execute(
                "UPDATE runs SET status = ?2, finished_unix_ms = ?3, monotonic_duration_ns = ?4,\n                    final_snapshot_id = ?5, exit_kind = ?6, exit_value = ?7\n                 WHERE id = ?1",
                params![
                    run_id.to_string(),
                    finish.status.as_str(),
                    finish.finished_at.as_unix_milliseconds(),
                    duration,
                    finish.final_snapshot.map(|id| id.to_string()),
                    exit_kind,
                    exit_value,
                ],
            )
            .map_err(|source| StoreError::Database {
                operation: "update finalized run",
                source,
            })?;
    Ok(())
}

fn run_select(suffix: &str) -> String {
    format!(
        "SELECT runs.id, runs.branch_id, run_parents.parent_run_id, run_parents.checkpoint_id,\n            runs.command, runs.workspace_root, runs.started_unix_ms, runs.finished_unix_ms,\n            runs.monotonic_duration_ns, runs.status, runs.platform, runs.record_input,\n            runs.capture_environment, runs.initial_snapshot_id, runs.final_snapshot_id,\n            runs.exit_kind, runs.exit_value\n         FROM runs LEFT JOIN run_parents ON run_parents.child_run_id = runs.id {suffix}"
    )
}

fn raw_run(row: &Row<'_>) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: row.get(0)?,
        branch_id: row.get(1)?,
        parent_run_id: row.get(2)?,
        parent_checkpoint_id: row.get(3)?,
        command: row.get(4)?,
        workspace_root: row.get(5)?,
        started_unix_ms: row.get(6)?,
        finished_unix_ms: row.get(7)?,
        monotonic_duration_ns: row.get(8)?,
        status: row.get(9)?,
        platform: row.get(10)?,
        record_input: row.get(11)?,
        capture_environment: row.get(12)?,
        initial_snapshot_id: row.get(13)?,
        final_snapshot_id: row.get(14)?,
        exit_kind: row.get(15)?,
        exit_value: row.get(16)?,
    })
}

fn decode_run(raw: RawRun, arguments: Vec<String>) -> Result<Run> {
    let id = parse_id("run ID", &raw.id)?;
    let parent = match (raw.parent_run_id, raw.parent_checkpoint_id) {
        (Some(run_id), Some(checkpoint_id)) => Some(RunParent {
            run_id: parse_id("parent run ID", &run_id)?,
            checkpoint_id: parse_id("parent checkpoint ID", &checkpoint_id)?,
        }),
        (None, None) => None,
        _ => {
            return Err(StoreError::InvalidRun {
                id: raw.id,
                message: "parent relationship is incomplete".to_owned(),
            });
        }
    };
    let duration = raw
        .monotonic_duration_ns
        .map(|value| decode_u64("run monotonic duration", value))
        .transpose()?
        .map(MonotonicDuration::from_nanoseconds);
    let run = Run {
        id,
        branch_id: parse_id::<BranchId>("branch ID", &raw.branch_id)?,
        parent,
        command: raw.command,
        arguments,
        workspace_root: PathBuf::from(raw.workspace_root),
        started_at: Timestamp::from_unix_milliseconds(raw.started_unix_ms),
        finished_at: raw.finished_unix_ms.map(Timestamp::from_unix_milliseconds),
        monotonic_duration: duration,
        status: parse_enum("run status", &raw.status)?,
        platform: parse_enum("platform", &raw.platform)?,
        capture_policy: CapturePolicy {
            record_input: parse_enum::<InputRecordingPolicy>(
                "input recording policy",
                &raw.record_input,
            )?,
            capture_environment: raw.capture_environment,
        },
        initial_snapshot: raw
            .initial_snapshot_id
            .map(|value| parse_id("initial snapshot ID", &value))
            .transpose()?,
        final_snapshot: raw
            .final_snapshot_id
            .map(|value| parse_id("final snapshot ID", &value))
            .transpose()?,
        exit_status: decode_exit_status(raw.exit_kind, raw.exit_value)?,
    };
    run.validate().map_err(|source| StoreError::InvalidRun {
        id: run.id.to_string(),
        message: source.to_string(),
    })?;
    Ok(run)
}

fn load_arguments(
    connection: &rusqlite::Connection,
    run_id: Option<RunId>,
) -> Result<BTreeMap<RunId, Vec<String>>> {
    let (sql, parameter) = match run_id {
        Some(id) => (
            "SELECT run_id, position, value FROM run_arguments WHERE run_id = ?1 ORDER BY run_id, position",
            Some(id.to_string()),
        ),
        None => (
            "SELECT run_id, position, value FROM run_arguments ORDER BY run_id, position",
            None,
        ),
    };
    let mut statement = connection
        .prepare(sql)
        .map_err(|source| StoreError::Database {
            operation: "prepare run arguments query",
            source,
        })?;
    let mut rows = if let Some(parameter) = parameter.as_deref() {
        statement.query([parameter])
    } else {
        statement.query([])
    }
    .map_err(|source| StoreError::Database {
        operation: "query run arguments",
        source,
    })?;
    let mut arguments: BTreeMap<RunId, Vec<String>> = BTreeMap::new();
    while let Some(row) = rows.next().map_err(|source| StoreError::Database {
        operation: "read run arguments",
        source,
    })? {
        let raw_id: String = row.get(0).map_err(|source| StoreError::Database {
            operation: "decode run argument owner",
            source,
        })?;
        let position: i64 = row.get(1).map_err(|source| StoreError::Database {
            operation: "decode run argument position",
            source,
        })?;
        let value: String = row.get(2).map_err(|source| StoreError::Database {
            operation: "decode run argument",
            source,
        })?;
        let id = parse_id("run ID", &raw_id)?;
        let values = arguments.entry(id).or_default();
        let expected = i64::try_from(values.len()).map_err(|_| StoreError::NumericRange {
            field: "run argument position",
            value: values.len() as u128,
        })?;
        if position != expected {
            return Err(StoreError::InvalidRun {
                id: raw_id,
                message: format!("argument position {position} followed {expected}"),
            });
        }
        values.push(value);
    }
    Ok(arguments)
}

fn reject_parent_cycle(transaction: &Transaction<'_>, child: RunId, parent: RunId) -> Result<()> {
    if child == parent {
        return Err(StoreError::ParentCycle {
            child: child.to_string(),
            parent: parent.to_string(),
        });
    }
    let would_cycle: bool = transaction
        .query_row(
            "WITH RECURSIVE ancestors(run_id) AS (\n                SELECT ?1\n                UNION\n                SELECT run_parents.parent_run_id FROM run_parents\n                JOIN ancestors ON run_parents.child_run_id = ancestors.run_id\n             )\n             SELECT EXISTS(SELECT 1 FROM ancestors WHERE run_id = ?2)",
            params![parent.to_string(), child.to_string()],
            |row| row.get(0),
        )
        .map_err(|source| StoreError::Database {
            operation: "validate run parent ancestry",
            source,
        })?;
    if would_cycle {
        Err(StoreError::ParentCycle {
            child: child.to_string(),
            parent: parent.to_string(),
        })
    } else {
        Ok(())
    }
}

fn missing_or_state_error(
    connection: &rusqlite::Connection,
    run_id: RunId,
    required: &'static str,
) -> Result<StoreError> {
    let status: Option<String> = connection
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "diagnose run state transition",
            source,
        })?;
    Ok(match status {
        Some(status) => StoreError::InvalidRun {
            id: run_id.to_string(),
            message: format!("run must be {required}, but is {status}"),
        },
        None => StoreError::NotFound {
            entity: "run",
            id: run_id.to_string(),
        },
    })
}

fn encode_exit_status(status: Option<ProcessExitStatus>) -> (Option<&'static str>, Option<i32>) {
    match status {
        None => (None, None),
        Some(ProcessExitStatus::Code(value)) => (Some("code"), Some(value)),
        Some(ProcessExitStatus::Signal(value)) => (Some("signal"), Some(value)),
        Some(ProcessExitStatus::Unknown) => (Some("unknown"), None),
    }
}

fn decode_exit_status(
    kind: Option<String>,
    value: Option<i32>,
) -> Result<Option<ProcessExitStatus>> {
    match (kind.as_deref(), value) {
        (None, None) => Ok(None),
        (Some("code"), Some(value)) => Ok(Some(ProcessExitStatus::Code(value))),
        (Some("signal"), Some(value)) => Ok(Some(ProcessExitStatus::Signal(value))),
        (Some("unknown"), None) => Ok(Some(ProcessExitStatus::Unknown)),
        _ => Err(StoreError::InvalidEnum {
            kind: "process exit status",
            value: format!("kind={kind:?}, value={value:?}"),
        }),
    }
}

pub(crate) fn parse_id<T>(kind: &'static str, value: &str) -> Result<T>
where
    T: FromStr,
{
    value.parse().map_err(|_| StoreError::InvalidIdentifier {
        kind,
        value: value.to_owned(),
    })
}

pub(crate) fn parse_enum<T>(kind: &'static str, value: &str) -> Result<T>
where
    T: FromStr,
{
    value.parse().map_err(|_| StoreError::InvalidEnum {
        kind,
        value: value.to_owned(),
    })
}

pub(crate) fn decode_u64(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Invariant {
        message: format!("negative stored value {value} for {field}"),
    })
}
