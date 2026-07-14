use rewind_domain::{
    Event, EventId, EventPayload, EventSequence, MonotonicDuration, RecorderWarningCode, RunId,
    Timestamp,
};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

use crate::run::{decode_u64, parse_id};
use crate::{Result, Store, StoreError, sql_u64};

/// Hard upper bound for one timeline query, independent of caller input.
pub const MAX_TIMELINE_PAGE: u32 = 10_000;

/// A bounded event page. Sequence, never timestamp, defines its order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelinePage {
    /// Ordered events in this page.
    pub events: Vec<Event>,
    /// Whether at least one later event exists.
    pub has_more: bool,
}

struct RawEvent {
    run_id: String,
    sequence: i64,
    id: String,
    wall_unix_ms: i64,
    monotonic_offset_ns: i64,
    schema_version: u16,
    kind: String,
    payload_json: String,
}

impl Store {
    /// Appends one contiguous batch in a transaction.
    ///
    /// The first event in a run must be sequence one; retries with duplicate
    /// sequences fail visibly rather than silently discarding data.
    pub fn append_event_batch(&mut self, events: &[Event]) -> Result<()> {
        self.require_writer("append events")?;
        if events.is_empty() {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin event batch",
                source,
            })?;
        append_event_batch_in(&transaction, events)?;
        transaction.commit().map_err(|source| StoreError::Database {
            operation: "commit event batch",
            source,
        })
    }

    /// Loads a bounded page strictly after `after`, or from sequence one.
    pub fn load_timeline(
        &self,
        run_id: RunId,
        after: Option<EventSequence>,
        limit: u32,
    ) -> Result<TimelinePage> {
        if limit == 0 || limit > MAX_TIMELINE_PAGE {
            return Err(StoreError::InvalidPageLimit {
                requested: limit,
                maximum: MAX_TIMELINE_PAGE,
            });
        }
        let after = after.map_or(0, EventSequence::get);
        let after = sql_u64("timeline cursor", after)?;
        let query_limit = i64::from(limit) + 1;
        let mut statement = self
            .connection
            .prepare(
                "SELECT run_id, sequence, id, wall_unix_ms, monotonic_offset_ns, schema_version, kind, payload_json\n                 FROM events WHERE run_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
            )
            .map_err(|source| StoreError::Database {
                operation: "prepare timeline page",
                source,
            })?;
        let rows = statement
            .query_map(params![run_id.to_string(), after, query_limit], raw_event)
            .map_err(|source| StoreError::Database {
                operation: "query timeline page",
                source,
            })?;
        let capacity = usize::try_from(limit).map_err(|_| StoreError::NumericRange {
            field: "timeline page limit",
            value: u128::from(limit),
        })?;
        let mut events = Vec::with_capacity(capacity);
        for row in rows {
            events.push(decode_event(row.map_err(|source| {
                StoreError::Database {
                    operation: "read timeline page",
                    source,
                }
            })?)?);
        }
        let has_more = events.len() > capacity;
        events.truncate(capacity);
        Ok(TimelinePage { events, has_more })
    }
}

pub(crate) fn append_event_batch_in(
    transaction: &rusqlite::Transaction<'_>,
    events: &[Event],
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    validate_batch(events)?;
    let run_id = events[0].run_id;
    let status: Option<String> = transaction
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "load event run status",
            source,
        })?;
    match status.as_deref() {
        Some("preparing" | "running") => {}
        Some(status) => {
            return Err(StoreError::EventOrder {
                message: format!("cannot append to {status} run {run_id}"),
            });
        }
        None => {
            return Err(StoreError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            });
        }
    }

    let previous: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT sequence, monotonic_offset_ns FROM events WHERE run_id = ?1 ORDER BY sequence DESC LIMIT 1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|source| StoreError::Database {
                operation: "load last event sequence",
                source,
            })?;
    let expected = match previous {
        Some((sequence, _)) => decode_u64("event sequence", sequence)?
            .checked_add(1)
            .ok_or_else(|| StoreError::EventOrder {
                message: format!("run {run_id} exhausted event sequence numbers"),
            })?,
        None => EventSequence::FIRST.get(),
    };
    if events[0].sequence.get() != expected {
        return Err(StoreError::EventOrder {
            message: format!(
                "run {run_id} expected sequence {expected}, received {}",
                events[0].sequence
            ),
        });
    }
    if let Some((_, offset)) = previous {
        let offset = decode_u64("event monotonic offset", offset)?;
        if events[0].monotonic_offset.as_nanoseconds() < offset {
            return Err(StoreError::EventOrder {
                message: format!(
                    "run {run_id} monotonic offset moved backward from {offset} to {}",
                    events[0].monotonic_offset
                ),
            });
        }
    }

    for event in events {
        let payload_json =
            serde_json::to_string(&event.payload).map_err(|source| StoreError::Serialization {
                context: "event payload",
                source,
            })?;
        transaction
                .execute(
                    "INSERT INTO events(\n                        run_id, sequence, id, wall_unix_ms, monotonic_offset_ns, schema_version, kind, payload_json\n                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        event.run_id.to_string(),
                        sql_u64("event sequence", event.sequence.get())?,
                        event.id.to_string(),
                        event.wall_clock.as_unix_milliseconds(),
                        sql_u64(
                            "event monotonic offset",
                            event.monotonic_offset.as_nanoseconds()
                        )?,
                        event.schema_version,
                        payload_kind(&event.payload),
                        payload_json,
                    ],
                )
                .map_err(|source| StoreError::Database {
                    operation: "insert event",
                    source,
                })?;
        validate_referenced_object(transaction, event)?;
        if let EventPayload::RecorderWarning { warning } = &event.payload {
            transaction
                .execute(
                    "INSERT INTO warnings(run_id, sequence, code, message, created_unix_ms)\n                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.run_id.to_string(),
                        sql_u64("warning event sequence", event.sequence.get())?,
                        warning_code(warning.code),
                        &warning.message,
                        event.wall_clock.as_unix_milliseconds(),
                    ],
                )
                .map_err(|source| StoreError::Database {
                    operation: "index recorder warning",
                    source,
                })?;
        }
        if let EventPayload::TerminalOutput {
            stream_id,
            object_id,
            byte_len,
        } = &event.payload
        {
            transaction
                    .execute(
                        "INSERT INTO terminal_chunks(\n                            run_id, stream_id, first_sequence, last_sequence, object_id, byte_length\n                         ) VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
                        params![
                            event.run_id.to_string(),
                            stream_id.to_string(),
                            sql_u64("terminal chunk sequence", event.sequence.get())?,
                            object_id.to_string(),
                            sql_u64("terminal chunk byte length", *byte_len)?,
                        ],
                    )
                    .map_err(|source| StoreError::Database {
                        operation: "index terminal output chunk",
                        source,
                    })?;
        }
    }
    Ok(())
}

fn validate_batch(events: &[Event]) -> Result<()> {
    let run_id = events[0].run_id;
    let mut previous_sequence: Option<u64> = None;
    let mut previous_offset: Option<u64> = None;
    for event in events {
        event
            .validate()
            .map_err(|source| StoreError::InvalidEvent {
                id: event.id.to_string(),
                message: source.to_string(),
            })?;
        if event.run_id != run_id {
            return Err(StoreError::EventOrder {
                message: format!("batch mixes runs {run_id} and {}", event.run_id),
            });
        }
        if let Some(previous) = previous_sequence
            && previous.checked_add(1) != Some(event.sequence.get())
        {
            return Err(StoreError::EventOrder {
                message: format!("sequence {previous} is followed by {}", event.sequence),
            });
        }
        if let Some(previous) = previous_offset
            && event.monotonic_offset.as_nanoseconds() < previous
        {
            return Err(StoreError::EventOrder {
                message: format!(
                    "monotonic offset {previous} is followed by {}",
                    event.monotonic_offset
                ),
            });
        }
        previous_sequence = Some(event.sequence.get());
        previous_offset = Some(event.monotonic_offset.as_nanoseconds());
    }
    Ok(())
}

fn validate_referenced_object(
    transaction: &rusqlite::Transaction<'_>,
    event: &Event,
) -> Result<()> {
    let reference = match &event.payload {
        EventPayload::TerminalInput {
            object_id,
            byte_len,
            ..
        }
        | EventPayload::TerminalOutput {
            object_id,
            byte_len,
            ..
        } => Some((*object_id, *byte_len)),
        EventPayload::RunStarted { .. }
        | EventPayload::WorkspaceIsolated { .. }
        | EventPayload::TerminalInputRedacted { .. }
        | EventPayload::TerminalResized { .. }
        | EventPayload::ProcessObserved { .. }
        | EventPayload::ProcessExited { .. }
        | EventPayload::FilesystemPathsDirtied { .. }
        | EventPayload::CheckpointStarted { .. }
        | EventPayload::CheckpointCommitted { .. }
        | EventPayload::CheckpointFailed { .. }
        | EventPayload::MarkerCreated { .. }
        | EventPayload::RunInterrupted { .. }
        | EventPayload::RunCompleted { .. }
        | EventPayload::RecorderWarning { .. } => None,
    };
    let Some((object_id, byte_len)) = reference else {
        return Ok(());
    };
    let recorded: Option<i64> = transaction
        .query_row(
            "SELECT logical_size FROM objects WHERE id = ?1",
            [object_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "validate terminal object reference",
            source,
        })?;
    let expected = sql_u64("terminal frame byte length", byte_len)?;
    if recorded == Some(expected) {
        Ok(())
    } else {
        Err(StoreError::InvalidEvent {
            id: event.id.to_string(),
            message: format!(
                "terminal object {object_id} has stored length {recorded:?}, expected {expected}"
            ),
        })
    }
}

fn payload_kind(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::RunStarted { .. } => "run_started",
        EventPayload::WorkspaceIsolated { .. } => "workspace_isolated",
        EventPayload::TerminalInput { .. } => "terminal_input",
        EventPayload::TerminalInputRedacted { .. } => "terminal_input_redacted",
        EventPayload::TerminalOutput { .. } => "terminal_output",
        EventPayload::TerminalResized { .. } => "terminal_resized",
        EventPayload::ProcessObserved { .. } => "process_observed",
        EventPayload::ProcessExited { .. } => "process_exited",
        EventPayload::FilesystemPathsDirtied { .. } => "filesystem_paths_dirtied",
        EventPayload::CheckpointStarted { .. } => "checkpoint_started",
        EventPayload::CheckpointCommitted { .. } => "checkpoint_committed",
        EventPayload::CheckpointFailed { .. } => "checkpoint_failed",
        EventPayload::MarkerCreated { .. } => "marker_created",
        EventPayload::RunInterrupted { .. } => "run_interrupted",
        EventPayload::RunCompleted { .. } => "run_completed",
        EventPayload::RecorderWarning { .. } => "recorder_warning",
    }
}

fn warning_code(code: RecorderWarningCode) -> &'static str {
    match code {
        RecorderWarningCode::CloneFallback => "clone_fallback",
        RecorderWarningCode::WatcherOverflow => "watcher_overflow",
        RecorderWarningCode::ProcessObservationIncomplete => "process_observation_incomplete",
        RecorderWarningCode::InputEchoDetectionUncertain => "input_echo_detection_uncertain",
        RecorderWarningCode::StorageLimit => "storage_limit",
        RecorderWarningCode::FilesystemRace => "filesystem_race",
        RecorderWarningCode::PrivacyCleanupFailed => "privacy_cleanup_failed",
        RecorderWarningCode::Other => "other",
    }
}

fn raw_event(row: &Row<'_>) -> rusqlite::Result<RawEvent> {
    Ok(RawEvent {
        run_id: row.get(0)?,
        sequence: row.get(1)?,
        id: row.get(2)?,
        wall_unix_ms: row.get(3)?,
        monotonic_offset_ns: row.get(4)?,
        schema_version: row.get(5)?,
        kind: row.get(6)?,
        payload_json: row.get(7)?,
    })
}

fn decode_event(raw: RawEvent) -> Result<Event> {
    let payload: EventPayload =
        serde_json::from_str(&raw.payload_json).map_err(|source| StoreError::Deserialization {
            context: "event payload",
            source,
        })?;
    if payload_kind(&payload) != raw.kind {
        return Err(StoreError::InvalidEvent {
            id: raw.id,
            message: format!(
                "stored kind {:?} disagrees with typed payload {:?}",
                raw.kind,
                payload_kind(&payload)
            ),
        });
    }
    let sequence =
        EventSequence::new(decode_u64("event sequence", raw.sequence)?).ok_or_else(|| {
            StoreError::InvalidEvent {
                id: raw.id.clone(),
                message: "event sequence is zero".to_owned(),
            }
        })?;
    let event = Event {
        id: parse_id::<EventId>("event ID", &raw.id)?,
        run_id: parse_id::<RunId>("event run ID", &raw.run_id)?,
        sequence,
        wall_clock: Timestamp::from_unix_milliseconds(raw.wall_unix_ms),
        monotonic_offset: MonotonicDuration::from_nanoseconds(decode_u64(
            "event monotonic offset",
            raw.monotonic_offset_ns,
        )?),
        schema_version: raw.schema_version,
        payload,
    };
    event
        .validate()
        .map_err(|source| StoreError::InvalidEvent {
            id: event.id.to_string(),
            message: source.to_string(),
        })?;
    Ok(event)
}
