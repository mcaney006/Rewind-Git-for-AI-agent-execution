use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    CheckpointId, CheckpointReason, EventId, EventSequence, MonotonicDuration, ObjectId,
    ProcessExitStatus, ProcessId, RunId, RunStatus, SnapshotId, SnapshotPath, TerminalStreamId,
    Timestamp,
};

/// The current durable event schema.
pub const EVENT_SCHEMA_VERSION: u16 = 1;

/// Maximum number of coalesced dirty paths in one event.
pub const MAX_DIRTY_PATHS_PER_EVENT: usize = 4096;

/// Maximum byte length of a human-readable event label or diagnostic.
pub const MAX_EVENT_TEXT_BYTES: usize = 8192;

/// A versioned, totally ordered execution event.
///
/// `sequence` is authoritative. Wall-clock timestamps may repeat or move
/// backward if the host clock is adjusted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// The globally unique event record identifier.
    pub id: EventId,
    /// The run that owns the event.
    pub run_id: RunId,
    /// The authoritative position within the run.
    pub sequence: EventSequence,
    /// Presentation timestamp in Unix epoch milliseconds.
    pub wall_clock: Timestamp,
    /// Monotonic offset from run start.
    pub monotonic_offset: MonotonicDuration,
    /// The payload schema used by this record.
    pub schema_version: u16,
    /// Typed event-specific data.
    pub payload: EventPayload,
}

impl Event {
    /// Creates and validates an event using the current schema and a fresh ID.
    pub fn new(
        run_id: RunId,
        sequence: EventSequence,
        wall_clock: Timestamp,
        monotonic_offset: MonotonicDuration,
        payload: EventPayload,
    ) -> Result<Self, EventValidationError> {
        let event = Self {
            id: EventId::generate(),
            run_id,
            sequence,
            wall_clock,
            monotonic_offset,
            schema_version: EVENT_SCHEMA_VERSION,
            payload,
        };
        event.validate()?;
        Ok(event)
    }

    /// Checks the schema and payload invariants before persistence.
    pub fn validate(&self) -> Result<(), EventValidationError> {
        if self.schema_version != EVENT_SCHEMA_VERSION {
            return Err(EventValidationError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: EVENT_SCHEMA_VERSION,
            });
        }
        self.payload
            .validate()
            .map_err(EventValidationError::InvalidPayload)
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventWire {
            id: EventId,
            run_id: RunId,
            sequence: EventSequence,
            wall_clock: Timestamp,
            monotonic_offset: MonotonicDuration,
            schema_version: u16,
            payload: EventPayload,
        }

        let wire = EventWire::deserialize(deserializer)?;
        let event = Self {
            id: wire.id,
            run_id: wire.run_id,
            sequence: wire.sequence,
            wall_clock: wire.wall_clock,
            monotonic_offset: wire.monotonic_offset,
            schema_version: wire.schema_version,
            payload: wire.payload,
        };
        event.validate().map_err(serde::de::Error::custom)?;
        Ok(event)
    }
}

/// Typed event data. No variant contains hidden model reasoning.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    /// Supervision and terminal capture have started.
    RunStarted {
        /// The supervised root process.
        root_process_id: ProcessId,
        /// The terminal byte stream connected to the process.
        terminal_stream_id: TerminalStreamId,
    },
    /// The source workspace was copied into the run's isolated workspace.
    WorkspaceIsolated {
        /// The effective clone strategy across the captured workspace.
        strategy: WorkspaceCloneStrategy,
    },
    /// Terminal input bytes were retained in an immutable object.
    TerminalInput {
        /// The terminal stream that received the bytes.
        stream_id: TerminalStreamId,
        /// The immutable raw input bytes.
        object_id: ObjectId,
        /// The exact number of raw bytes.
        byte_len: u64,
    },
    /// Terminal input occurred but its bytes were intentionally omitted.
    TerminalInputRedacted {
        /// The terminal stream that received the bytes.
        stream_id: TerminalStreamId,
        /// The omitted raw byte count.
        byte_len: u64,
        /// Why the bytes were not retained.
        reason: InputRedactionReason,
    },
    /// Raw terminal output was durably streamed into an immutable object.
    TerminalOutput {
        /// The terminal stream that produced the bytes.
        stream_id: TerminalStreamId,
        /// The immutable raw output bytes, including ANSI sequences.
        object_id: ObjectId,
        /// The exact number of raw bytes.
        byte_len: u64,
    },
    /// The supervised terminal size changed.
    TerminalResized {
        /// The resized terminal stream.
        stream_id: TerminalStreamId,
        /// The nonzero terminal width in character cells.
        columns: u16,
        /// The nonzero terminal height in character cells.
        rows: u16,
    },
    /// A member of the supervised process tree was observed.
    ProcessObserved {
        /// Best-effort process metadata.
        process: ProcessObservation,
    },
    /// A supervised process termination was observed.
    ProcessExited {
        /// The terminated process.
        process_id: ProcessId,
        /// The best available exit status.
        status: ProcessExitStatus,
    },
    /// Watcher hints identified likely changed workspace paths.
    FilesystemPathsDirtied {
        /// Sorted, unique normalized paths. These remain hints, not truth.
        paths: Vec<SnapshotPath>,
    },
    /// Snapshot work began for a checkpoint request.
    CheckpointStarted {
        /// The checkpoint being prepared.
        checkpoint_id: CheckpointId,
        /// Why the checkpoint was requested.
        reason: CheckpointReason,
    },
    /// A checkpoint and its snapshot committed atomically.
    CheckpointCommitted {
        /// The committed checkpoint.
        checkpoint_id: CheckpointId,
        /// The committed workspace identity.
        snapshot_id: SnapshotId,
    },
    /// A checkpoint attempt failed without becoming visible as committed.
    CheckpointFailed {
        /// The failed checkpoint request.
        checkpoint_id: CheckpointId,
        /// A safe structured failure description.
        failure: RecorderFailure,
    },
    /// A user marker was accepted for checkpoint creation.
    MarkerCreated {
        /// The checkpoint allocated for the marker.
        checkpoint_id: CheckpointId,
        /// The nonempty user-facing marker text.
        label: String,
    },
    /// An interruption began orderly recorder shutdown.
    RunInterrupted {
        /// The initiating signal number, if observed.
        signal: Option<i32>,
    },
    /// The recorder reached a terminal run state.
    RunCompleted {
        /// The terminal lifecycle status.
        status: RunStatus,
        /// The root process result, when available.
        exit_status: Option<ProcessExitStatus>,
    },
    /// Capture continued with a visible limitation or degradation.
    RecorderWarning {
        /// A safe, structured warning.
        warning: RecorderWarning,
    },
}

impl EventPayload {
    /// Builds a canonical dirty-path event by sorting and deduplicating hints.
    pub fn filesystem_paths_dirtied(
        mut paths: Vec<SnapshotPath>,
    ) -> Result<Self, EventPayloadError> {
        paths.sort();
        paths.dedup();
        let payload = Self::FilesystemPathsDirtied { paths };
        payload.validate()?;
        Ok(payload)
    }

    /// Checks bounded-size and cross-field payload invariants.
    pub fn validate(&self) -> Result<(), EventPayloadError> {
        match self {
            Self::TerminalInput { byte_len, .. }
            | Self::TerminalInputRedacted { byte_len, .. }
            | Self::TerminalOutput { byte_len, .. }
                if *byte_len == 0 =>
            {
                Err(EventPayloadError::EmptyTerminalFrame)
            }
            Self::TerminalResized { columns, rows, .. } if *columns == 0 || *rows == 0 => {
                Err(EventPayloadError::ZeroTerminalDimension)
            }
            Self::ProcessObserved { process } => process.validate(),
            Self::FilesystemPathsDirtied { paths } => validate_dirty_paths(paths),
            Self::MarkerCreated { label, .. } if label.is_empty() => {
                Err(EventPayloadError::EmptyMarkerLabel)
            }
            Self::MarkerCreated { label, .. } if label.len() > MAX_EVENT_TEXT_BYTES => {
                Err(EventPayloadError::TextTooLong)
            }
            Self::CheckpointFailed { failure, .. } => failure.validate(),
            Self::RunCompleted { status, .. } if !status.is_terminal() => {
                Err(EventPayloadError::NonterminalCompletionStatus)
            }
            Self::RecorderWarning { warning } => warning.validate(),
            Self::RunStarted { .. }
            | Self::WorkspaceIsolated { .. }
            | Self::TerminalInput { .. }
            | Self::TerminalInputRedacted { .. }
            | Self::TerminalOutput { .. }
            | Self::TerminalResized { .. }
            | Self::ProcessExited { .. }
            | Self::CheckpointStarted { .. }
            | Self::CheckpointCommitted { .. }
            | Self::MarkerCreated { .. }
            | Self::RunInterrupted { .. }
            | Self::RunCompleted { .. } => Ok(()),
        }
    }
}

/// The durable, platform-neutral workspace clone result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCloneStrategy {
    /// Every regular file used APFS descriptor-relative cloning.
    ApfsClone,
    /// Every regular file used Linux `FICLONE`.
    LinuxReflink,
    /// Some regular files cloned and others required byte copies.
    Mixed,
    /// Every regular file required a byte copy.
    RecursiveCopy,
}

impl WorkspaceCloneStrategy {
    /// Returns the stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApfsClone => "apfs_clone",
            Self::LinuxReflink => "linux_reflink",
            Self::Mixed => "mixed",
            Self::RecursiveCopy => "recursive_copy",
        }
    }
}

impl fmt::Display for WorkspaceCloneStrategy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate_dirty_paths(paths: &[SnapshotPath]) -> Result<(), EventPayloadError> {
    if paths.is_empty() {
        return Err(EventPayloadError::EmptyDirtyPaths);
    }
    if paths.len() > MAX_DIRTY_PATHS_PER_EVENT {
        return Err(EventPayloadError::TooManyDirtyPaths {
            actual: paths.len(),
            maximum: MAX_DIRTY_PATHS_PER_EVENT,
        });
    }
    if paths.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(EventPayloadError::NonCanonicalDirtyPaths);
    }
    Ok(())
}

/// Why input bytes were omitted from an otherwise complete terminal timeline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRedactionReason {
    /// The child terminal reported that echo was disabled.
    EchoDisabled,
    /// The effective policy prohibited all input recording.
    PolicyNever,
    /// Echo state could not be assessed safely, so capture chose redaction.
    EchoDetectionUnavailable,
    /// A local export policy omitted bytes retained in the private source run.
    ExportPolicy,
}

/// How an observed process relates to the supervised root.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRelationship {
    /// The process launched directly by Rewind.
    Root,
    /// A process observed beneath the root.
    Descendant,
}

/// Best-effort metadata for one member of the supervised process tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessObservation {
    /// The observed process ID.
    pub process_id: ProcessId,
    /// The parent process ID when observable.
    pub parent_process_id: Option<ProcessId>,
    /// An executable path when observable and safely representable.
    pub executable: Option<String>,
    /// The nonempty observed command name.
    pub command: String,
    /// Its relationship to the supervised root.
    pub relationship: ProcessRelationship,
}

impl ProcessObservation {
    /// Checks bounded textual process metadata.
    pub fn validate(&self) -> Result<(), EventPayloadError> {
        if self.command.is_empty() {
            return Err(EventPayloadError::EmptyProcessCommand);
        }
        if self.command.len() > MAX_EVENT_TEXT_BYTES
            || self
                .executable
                .as_ref()
                .is_some_and(|value| value.len() > MAX_EVENT_TEXT_BYTES)
        {
            return Err(EventPayloadError::TextTooLong);
        }
        Ok(())
    }
}

/// A stable category for a failed recorder operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderFailureKind {
    /// Workspace snapshot creation failed.
    Snapshot,
    /// Durable metadata or object persistence failed.
    Storage,
    /// A workspace changed concurrently beyond the supported retry policy.
    ConcurrentWorkspaceMutation,
    /// A configured resource limit was reached.
    ResourceLimit,
    /// An internal invariant was violated.
    InternalInvariant,
}

/// A safe failure description suitable for durable replay metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderFailure {
    /// The stable failure category.
    pub kind: RecorderFailureKind,
    /// A concise non-secret explanation.
    pub message: String,
}

impl RecorderFailure {
    fn validate(&self) -> Result<(), EventPayloadError> {
        validate_diagnostic(&self.message)
    }
}

/// A stable category for nonfatal recording limitations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderWarningCode {
    /// Copy-on-write cloning was unavailable and recursive copy was used.
    CloneFallback,
    /// A watcher overflow means dirty hints may be incomplete.
    WatcherOverflow,
    /// Short-lived or inaccessible process metadata may be incomplete.
    ProcessObservationIncomplete,
    /// Terminal echo detection could not provide certainty.
    InputEchoDetectionUncertain,
    /// A configured storage threshold is near or at its limit.
    StorageLimit,
    /// The authoritative scan observed a concurrent filesystem race.
    FilesystemRace,
    /// Excluded content could not be removed from the retained run workspace.
    PrivacyCleanupFailed,
    /// A warning not represented by an earlier stable category.
    Other,
}

/// A safe nonfatal warning visible during replay and comparison.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderWarning {
    /// The stable warning category.
    pub code: RecorderWarningCode,
    /// A concise non-secret explanation.
    pub message: String,
}

impl RecorderWarning {
    fn validate(&self) -> Result<(), EventPayloadError> {
        validate_diagnostic(&self.message)
    }
}

fn validate_diagnostic(message: &str) -> Result<(), EventPayloadError> {
    if message.is_empty() {
        return Err(EventPayloadError::EmptyDiagnostic);
    }
    if message.len() > MAX_EVENT_TEXT_BYTES {
        return Err(EventPayloadError::TextTooLong);
    }
    Ok(())
}

/// A violated event payload invariant.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EventPayloadError {
    /// Zero-byte terminal frames carry no timing or content information.
    #[error("terminal frame byte length must be greater than zero")]
    EmptyTerminalFrame,
    /// PTY rows and columns are both nonzero.
    #[error("terminal rows and columns must be greater than zero")]
    ZeroTerminalDimension,
    /// Dirty-path events must identify at least one path.
    #[error("filesystem dirty-path event must not be empty")]
    EmptyDirtyPaths,
    /// One event exceeded the bounded path batch.
    #[error("filesystem dirty-path event contains {actual} paths; maximum is {maximum}")]
    TooManyDirtyPaths {
        /// The path count supplied.
        actual: usize,
        /// The durable event maximum.
        maximum: usize,
    },
    /// Persisted dirty paths must be strictly sorted and unique.
    #[error("filesystem dirty paths must be sorted and unique")]
    NonCanonicalDirtyPaths,
    /// User marker text cannot be empty.
    #[error("marker label must not be empty")]
    EmptyMarkerLabel,
    /// Process observations require a command name.
    #[error("observed process command must not be empty")]
    EmptyProcessCommand,
    /// Diagnostics and failures require visible text.
    #[error("recorder diagnostic text must not be empty")]
    EmptyDiagnostic,
    /// Bounded durable metadata rejects oversized text.
    #[error("event text exceeds the supported byte length")]
    TextTooLong,
    /// A completion event must carry a terminal lifecycle state.
    #[error("run completion event must contain a terminal run status")]
    NonterminalCompletionStatus,
}

/// A rejected durable event record.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EventValidationError {
    /// Only explicitly understood event schemas may be replayed.
    #[error("unsupported event schema version {found}; supported version is {supported}")]
    UnsupportedSchemaVersion {
        /// The event's version.
        found: u16,
        /// The sole version understood by this crate.
        supported: u16,
    },
    /// The typed payload violates a durable invariant.
    #[error("invalid event payload: {0}")]
    InvalidPayload(#[source] EventPayloadError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64, timestamp: i64, payload: EventPayload) -> Event {
        Event::new(
            RunId::generate(),
            EventSequence::new(sequence).unwrap(),
            Timestamp::from_unix_milliseconds(timestamp),
            MonotonicDuration::from_nanoseconds(sequence),
            payload,
        )
        .unwrap()
    }

    #[test]
    fn sequence_order_is_independent_of_wall_clock() {
        let stream_id = TerminalStreamId::generate();
        let mut events = [
            event(
                3,
                -100,
                EventPayload::TerminalOutput {
                    stream_id,
                    object_id: ObjectId::digest(b"c"),
                    byte_len: 1,
                },
            ),
            event(
                1,
                500,
                EventPayload::TerminalOutput {
                    stream_id,
                    object_id: ObjectId::digest(b"a"),
                    byte_len: 1,
                },
            ),
            event(
                2,
                500,
                EventPayload::TerminalOutput {
                    stream_id,
                    object_id: ObjectId::digest(b"b"),
                    byte_len: 1,
                },
            ),
        ];
        events.sort_by_key(|value| value.sequence);
        assert_eq!(
            events
                .iter()
                .map(|value| value.sequence.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn dirty_paths_are_canonicalized_and_bounded() {
        let payload = EventPayload::filesystem_paths_dirtied(vec![
            "z".parse().unwrap(),
            "a".parse().unwrap(),
            "z".parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            payload,
            EventPayload::FilesystemPathsDirtied {
                paths: vec!["a".parse().unwrap(), "z".parse().unwrap()]
            }
        );
        assert!(EventPayload::filesystem_paths_dirtied(Vec::new()).is_err());
    }

    #[test]
    fn event_serde_preserves_typed_payload_and_rejects_unknown_schema() {
        let checkpoint_id = CheckpointId::generate();
        let original = event(
            1,
            10,
            EventPayload::CheckpointCommitted {
                checkpoint_id,
                snapshot_id: SnapshotId::digest(b"manifest"),
            },
        );
        let encoded = serde_json::to_vec(&original).unwrap();
        assert_eq!(serde_json::from_slice::<Event>(&encoded).unwrap(), original);

        let mut value = serde_json::to_value(&original).unwrap();
        value["schema_version"] = serde_json::json!(2);
        assert!(serde_json::from_value::<Event>(value).is_err());
    }

    #[test]
    fn workspace_clone_strategy_has_stable_wire_spelling() {
        let payload = EventPayload::WorkspaceIsolated {
            strategy: WorkspaceCloneStrategy::RecursiveCopy,
        };
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            serde_json::json!({
                "type": "workspace_isolated",
                "data": { "strategy": "recursive_copy" }
            })
        );
    }

    #[test]
    fn payload_validation_catches_privacy_metadata_errors() {
        let stream_id = TerminalStreamId::generate();
        assert_eq!(
            EventPayload::TerminalInputRedacted {
                stream_id,
                byte_len: 0,
                reason: InputRedactionReason::EchoDisabled,
            }
            .validate(),
            Err(EventPayloadError::EmptyTerminalFrame)
        );
        assert_eq!(
            EventPayload::RunCompleted {
                status: RunStatus::Running,
                exit_status: None,
            }
            .validate(),
            Err(EventPayloadError::NonterminalCompletionStatus)
        );
    }
}
