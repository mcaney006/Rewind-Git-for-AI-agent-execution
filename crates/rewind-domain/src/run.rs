use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    BranchId, CheckpointId, EventSequence, MonotonicDuration, RunId, SnapshotId, Timestamp,
};

/// A supported host operating-system and architecture pair.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// macOS on Apple Silicon.
    MacOsAarch64,
    /// Linux on the x86-64 architecture.
    LinuxX86_64,
    /// Linux on the ARM64 architecture.
    LinuxAarch64,
}

impl Platform {
    /// Returns the stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacOsAarch64 => "macos_aarch64",
            Self::LinuxX86_64 => "linux_x86_64",
            Self::LinuxAarch64 => "linux_aarch64",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = EnumParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "macos_aarch64" => Ok(Self::MacOsAarch64),
            "linux_x86_64" => Ok(Self::LinuxX86_64),
            "linux_aarch64" => Ok(Self::LinuxAarch64),
            _ => Err(EnumParseError::new("platform", input)),
        }
    }
}

/// Policy controlling whether terminal input bytes are retained.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum InputRecordingPolicy {
    /// Record input only while the terminal reports that echo is enabled.
    #[default]
    Auto,
    /// Record input regardless of the observed echo state.
    Always,
    /// Never retain terminal input bytes.
    Never,
}

impl InputRecordingPolicy {
    /// Returns the stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

impl fmt::Display for InputRecordingPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for InputRecordingPolicy {
    type Err = EnumParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => Err(EnumParseError::new("input recording policy", input)),
        }
    }
}

/// Effective privacy-relevant capture choices recorded with a run.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturePolicy {
    /// The terminal input retention policy.
    pub record_input: InputRecordingPolicy,
    /// Whether an explicit environment allowlist was captured.
    pub capture_environment: bool,
}

/// The lifecycle state of a recorded run.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Metadata exists but capture has not started.
    Preparing,
    /// The command and recorder are active.
    Running,
    /// The command and recorder completed normally.
    Completed,
    /// The command or recorder reported a failure.
    Failed,
    /// Recording ended because an interruption was requested or recovered.
    Interrupted,
    /// Recording ended unexpectedly without an orderly shutdown.
    Crashed,
}

impl RunStatus {
    /// Returns whether `next` is a legal one-way lifecycle transition.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Preparing,
                Self::Running | Self::Failed | Self::Interrupted | Self::Crashed
            ) | (
                Self::Running,
                Self::Completed | Self::Failed | Self::Interrupted | Self::Crashed
            )
        )
    }

    /// Applies a legal transition and leaves the value unchanged on failure.
    pub fn transition_to(&mut self, next: Self) -> Result<(), InvalidRunStatusTransition> {
        if !self.can_transition_to(next) {
            return Err(InvalidRunStatusTransition {
                from: *self,
                to: next,
            });
        }
        *self = next;
        Ok(())
    }

    /// Returns whether no further lifecycle transition is permitted.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::Crashed
        )
    }

    /// Returns the stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Crashed => "crashed",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RunStatus {
    type Err = EnumParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "preparing" => Ok(Self::Preparing),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            "crashed" => Ok(Self::Crashed),
            _ => Err(EnumParseError::new("run status", input)),
        }
    }
}

/// A rejected run lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("illegal run status transition from {from} to {to}")]
pub struct InvalidRunStatusTransition {
    /// The current state.
    pub from: RunStatus,
    /// The requested next state.
    pub to: RunStatus,
}

/// A child process's observed termination result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProcessExitStatus {
    /// The process exited and returned this numeric code.
    Code(i32),
    /// The process was terminated by this numeric signal.
    Signal(i32),
    /// No reliable termination detail was observable.
    Unknown,
}

impl ProcessExitStatus {
    /// Returns the exit code when the process exited normally.
    #[must_use]
    pub const fn code(self) -> Option<i32> {
        match self {
            Self::Code(code) => Some(code),
            Self::Signal(_) | Self::Unknown => None,
        }
    }

    /// Reports observed success, or `None` when the result was unknown.
    #[must_use]
    pub const fn success(self) -> Option<bool> {
        match self {
            Self::Code(code) => Some(code == 0),
            Self::Signal(_) => Some(false),
            Self::Unknown => None,
        }
    }
}

/// The durable origin of a forked run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunParent {
    /// The direct parent run.
    pub run_id: RunId,
    /// The parent checkpoint materialized for the child.
    pub checkpoint_id: CheckpointId,
}

/// Durable metadata for one supervised execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// The run identifier.
    pub id: RunId,
    /// The logical execution branch containing this run.
    pub branch_id: BranchId,
    /// The origin checkpoint for a fork, if any.
    pub parent: Option<RunParent>,
    /// The executable or command name exactly as launched.
    pub command: String,
    /// Command arguments, excluding the executable itself.
    pub arguments: Vec<String>,
    /// The isolated workspace in which the command ran.
    pub workspace_root: PathBuf,
    /// The wall-clock start time.
    pub started_at: Timestamp,
    /// The wall-clock finish time after a terminal state is reached.
    pub finished_at: Option<Timestamp>,
    /// The monotonic elapsed duration after a terminal state is reached.
    pub monotonic_duration: Option<MonotonicDuration>,
    /// The lifecycle state.
    pub status: RunStatus,
    /// The host platform used for capture.
    pub platform: Platform,
    /// Effective capture policy recorded for later inspection.
    pub capture_policy: CapturePolicy,
    /// The isolated workspace state before the command started.
    pub initial_snapshot: Option<SnapshotId>,
    /// The authoritative final workspace state, when one was committed.
    pub final_snapshot: Option<SnapshotId>,
    /// The observed root process result, when available.
    pub exit_status: Option<ProcessExitStatus>,
}

impl Run {
    /// Checks cross-field lifecycle invariants before persistence.
    pub fn validate(&self) -> Result<(), RunValidationError> {
        if self.command.is_empty() {
            return Err(RunValidationError::EmptyCommand);
        }
        if self.parent.is_some_and(|parent| parent.run_id == self.id) {
            return Err(RunValidationError::SelfParent);
        }
        if self.status == RunStatus::Preparing {
            if self.finished_at.is_some() || self.monotonic_duration.is_some() {
                return Err(RunValidationError::ActiveRunHasFinishMetadata);
            }
        } else if self.status == RunStatus::Running {
            if self.initial_snapshot.is_none() {
                return Err(RunValidationError::RunningWithoutInitialSnapshot);
            }
            if self.finished_at.is_some() || self.monotonic_duration.is_some() {
                return Err(RunValidationError::ActiveRunHasFinishMetadata);
            }
        } else if self.finished_at.is_none() || self.monotonic_duration.is_none() {
            return Err(RunValidationError::TerminalRunMissingFinishMetadata);
        }
        Ok(())
    }
}

/// A violated cross-field run invariant.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RunValidationError {
    /// A run cannot launch an empty executable name.
    #[error("run command must not be empty")]
    EmptyCommand,
    /// A run cannot be its own direct parent.
    #[error("run cannot name itself as its parent")]
    SelfParent,
    /// Preparing and running records cannot already contain finish metadata.
    #[error("active run must not contain finish metadata")]
    ActiveRunHasFinishMetadata,
    /// Capture cannot enter `Running` before its initial snapshot exists.
    #[error("running run must reference an initial snapshot")]
    RunningWithoutInitialSnapshot,
    /// Every terminal record needs both wall-clock and monotonic finish data.
    #[error("terminal run must contain finish time and duration")]
    TerminalRunMissingFinishMetadata,
}

/// Why a workspace checkpoint was requested.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// The isolated workspace state before command execution.
    Initial,
    /// An explicit user marker or control request.
    Manual,
    /// A useful supervised process lifecycle boundary.
    ProcessBoundary,
    /// A quiescent interval following filesystem changes.
    FilesystemQuiescence,
    /// A boundary intentionally exposed by an optional agent adapter.
    AgentAdapter,
    /// The authoritative scan performed during finalization.
    Final,
}

impl CheckpointReason {
    /// Returns the stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Manual => "manual",
            Self::ProcessBoundary => "process_boundary",
            Self::FilesystemQuiescence => "filesystem_quiescence",
            Self::AgentAdapter => "agent_adapter",
            Self::Final => "final",
        }
    }
}

impl fmt::Display for CheckpointReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CheckpointReason {
    type Err = EnumParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "initial" => Ok(Self::Initial),
            "manual" => Ok(Self::Manual),
            "process_boundary" => Ok(Self::ProcessBoundary),
            "filesystem_quiescence" => Ok(Self::FilesystemQuiescence),
            "agent_adapter" => Ok(Self::AgentAdapter),
            "final" => Ok(Self::Final),
            _ => Err(EnumParseError::new("checkpoint reason", input)),
        }
    }
}

/// A committed workspace checkpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    /// The checkpoint identifier.
    pub id: CheckpointId,
    /// The owning run.
    pub run_id: RunId,
    /// The authoritative event position at which commit completed.
    pub sequence: EventSequence,
    /// An optional human-readable marker label.
    pub label: Option<String>,
    /// The trigger that requested this checkpoint.
    pub reason: CheckpointReason,
    /// The committed workspace snapshot.
    pub snapshot_id: SnapshotId,
    /// The wall-clock commit time.
    pub created_at: Timestamp,
    /// The monotonic offset from the run start.
    pub monotonic_offset: MonotonicDuration,
}

impl Checkpoint {
    /// Rejects an empty marker label while preserving the distinction from no label.
    pub fn validate(&self) -> Result<(), CheckpointValidationError> {
        if self.label.as_deref() == Some("") {
            return Err(CheckpointValidationError::EmptyLabel);
        }
        Ok(())
    }
}

/// A violated committed-checkpoint invariant.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CheckpointValidationError {
    /// Empty text is not a meaningful manual marker.
    #[error("checkpoint label must be absent or nonempty")]
    EmptyLabel,
}

/// Failure to parse a stable durable enum spelling.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unknown {kind} value {value:?}")]
pub struct EnumParseError {
    kind: &'static str,
    value: String,
}

impl EnumParseError {
    fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_owned(),
        }
    }

    /// Returns the kind of value that failed to parse.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    /// Returns the rejected input.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_transition_matrix_is_closed_and_one_way() {
        let statuses = [
            RunStatus::Preparing,
            RunStatus::Running,
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Crashed,
        ];
        for from in statuses {
            for to in statuses {
                let expected = matches!(
                    (from, to),
                    (
                        RunStatus::Preparing,
                        RunStatus::Running
                            | RunStatus::Failed
                            | RunStatus::Interrupted
                            | RunStatus::Crashed
                    ) | (
                        RunStatus::Running,
                        RunStatus::Completed
                            | RunStatus::Failed
                            | RunStatus::Interrupted
                            | RunStatus::Crashed
                    )
                );
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
            }
        }
    }

    #[test]
    fn rejected_transition_does_not_mutate_state() {
        let mut status = RunStatus::Completed;
        assert!(status.transition_to(RunStatus::Running).is_err());
        assert_eq!(status, RunStatus::Completed);
    }

    #[test]
    fn stable_enum_spellings_round_trip() {
        for status in [
            RunStatus::Preparing,
            RunStatus::Running,
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Crashed,
        ] {
            assert_eq!(status.to_string().parse::<RunStatus>().unwrap(), status);
            assert_eq!(
                serde_json::from_str::<RunStatus>(&serde_json::to_string(&status).unwrap())
                    .unwrap(),
                status
            );
        }
        assert!("Completed".parse::<RunStatus>().is_err());
    }
}
