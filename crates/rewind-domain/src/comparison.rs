use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    CheckpointId, MonotonicDuration, ProcessExitStatus, RunId, RunStatus, SnapshotId, SnapshotPath,
    UnixPermissions,
};

/// The structural relationship found between two runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunRelationship {
    /// Both arguments identify the same run.
    SameRun,
    /// The left run is the direct parent of the right run.
    LeftParentOfRight {
        /// The left checkpoint used to start the right run.
        fork_checkpoint: CheckpointId,
    },
    /// The right run is the direct parent of the left run.
    RightParentOfLeft {
        /// The right checkpoint used to start the left run.
        fork_checkpoint: CheckpointId,
    },
    /// The runs share an ancestor but neither directly parents the other.
    CommonAncestor {
        /// The nearest common ancestor found by the comparison query.
        run_id: RunId,
    },
    /// No durable ancestry relationship was found.
    Unrelated,
}

/// Starting or final snapshot identities for the compared sides.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotComparison {
    /// The left snapshot, if the run committed one.
    pub left: Option<SnapshotId>,
    /// The right snapshot, if the run committed one.
    pub right: Option<SnapshotId>,
}

/// Evidence about one side of a run comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunComparisonSummary {
    /// The run being summarized.
    pub run_id: RunId,
    /// Its final or current lifecycle status.
    pub status: RunStatus,
    /// The supervised root result, when observed.
    pub exit_status: Option<ProcessExitStatus>,
    /// Monotonic run duration, absent for a still-active run.
    pub duration: Option<MonotonicDuration>,
    /// Total raw terminal output bytes retained for the run.
    pub terminal_output_bytes: u64,
    /// Number of committed checkpoints.
    pub checkpoint_count: u64,
    /// Number of durable recorder warnings.
    pub warning_count: u64,
}

/// The filesystem entry type used in a concise diff.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotEntryType {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
}

/// A before-and-after pair with no implied preference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueChange<T> {
    /// The left-side value.
    pub left: T,
    /// The right-side value.
    pub right: T,
}

/// Evidence describing one changed workspace path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChangeKind {
    /// A path exists only on the right.
    Added {
        /// The right-side entry type.
        entry_type: SnapshotEntryType,
        /// The executable state for a regular file.
        executable: Option<bool>,
    },
    /// A path exists only on the left.
    Deleted {
        /// The left-side entry type.
        entry_type: SnapshotEntryType,
        /// The executable state for a regular file.
        executable: Option<bool>,
    },
    /// Both snapshots contain the path, with at least one evidenced difference.
    Modified {
        /// Whether the stored file or symlink target bytes differ.
        content_changed: bool,
        /// A type transition, when one occurred.
        entry_type: Option<ValueChange<SnapshotEntryType>>,
        /// An executable-bit transition for a regular file.
        executable: Option<ValueChange<bool>>,
        /// A supported Unix permission transition.
        permissions: Option<ValueChange<UnixPermissions>>,
    },
}

impl FileChangeKind {
    fn validate(&self) -> Result<(), ComparisonValidationError> {
        match self {
            Self::Modified {
                content_changed: false,
                entry_type: None,
                executable: None,
                permissions: None,
            } => Err(ComparisonValidationError::EmptyModification),
            Self::Added {
                entry_type,
                executable,
            }
            | Self::Deleted {
                entry_type,
                executable,
            } if (*entry_type == SnapshotEntryType::File) != executable.is_some() => {
                Err(ComparisonValidationError::InvalidExecutableEvidence)
            }
            Self::Added { .. } | Self::Deleted { .. } | Self::Modified { .. } => Ok(()),
        }
    }
}

/// One changed normalized workspace path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    /// The normalized path common to both snapshot roots.
    pub path: SnapshotPath,
    /// Filesystem evidence for the change.
    pub change: FileChangeKind,
}

/// The evidence produced by one optional isolated evaluation command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    /// The exact evaluation command presented to the local shell.
    pub command: String,
    /// The observed process result.
    pub exit_status: ProcessExitStatus,
    /// The monotonic evaluation duration.
    pub duration: MonotonicDuration,
}

/// Evaluation evidence from independently materialized final snapshots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationComparison {
    /// Evaluation performed in the left final snapshot.
    pub left: EvaluationResult,
    /// Evaluation performed in the right final snapshot.
    pub right: EvaluationResult,
}

/// A deterministic evidence report with no synthetic winner or quality score.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonResult {
    /// Metrics and outcome evidence for the left run.
    pub left: RunComparisonSummary,
    /// Metrics and outcome evidence for the right run.
    pub right: RunComparisonSummary,
    /// The durable branch relationship between the runs.
    pub relationship: RunRelationship,
    /// The runs' initial snapshots.
    pub starting_snapshots: SnapshotComparison,
    /// The runs' final snapshots.
    pub final_snapshots: SnapshotComparison,
    file_changes: Vec<FileChange>,
    /// Optional test evidence produced in two isolated materializations.
    pub evaluation: Option<EvaluationComparison>,
}

impl ComparisonResult {
    /// Builds a report after sorting changed paths into canonical order.
    pub fn new(
        left: RunComparisonSummary,
        right: RunComparisonSummary,
        relationship: RunRelationship,
        starting_snapshots: SnapshotComparison,
        final_snapshots: SnapshotComparison,
        mut file_changes: Vec<FileChange>,
        evaluation: Option<EvaluationComparison>,
    ) -> Result<Self, ComparisonValidationError> {
        file_changes.sort_by(|left, right| left.path.cmp(&right.path));
        Self::from_canonical_parts(
            left,
            right,
            relationship,
            starting_snapshots,
            final_snapshots,
            file_changes,
            evaluation,
        )
    }

    fn from_canonical_parts(
        left: RunComparisonSummary,
        right: RunComparisonSummary,
        relationship: RunRelationship,
        starting_snapshots: SnapshotComparison,
        final_snapshots: SnapshotComparison,
        file_changes: Vec<FileChange>,
        evaluation: Option<EvaluationComparison>,
    ) -> Result<Self, ComparisonValidationError> {
        for (index, change) in file_changes.iter().enumerate() {
            change.change.validate()?;
            if let Some(previous) = index
                .checked_sub(1)
                .and_then(|value| file_changes.get(value))
            {
                match previous.path.cmp(&change.path) {
                    std::cmp::Ordering::Equal => {
                        return Err(ComparisonValidationError::DuplicatePath {
                            path: change.path.clone(),
                        });
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(ComparisonValidationError::NonCanonicalOrder { index });
                    }
                    std::cmp::Ordering::Less => {}
                }
            }
        }
        if evaluation.as_ref().is_some_and(|value| {
            value.left.command.is_empty()
                || value.right.command.is_empty()
                || value.left.command != value.right.command
        }) {
            return Err(ComparisonValidationError::InvalidEvaluationCommands);
        }
        Ok(Self {
            left,
            right,
            relationship,
            starting_snapshots,
            final_snapshots,
            file_changes,
            evaluation,
        })
    }

    /// Borrows changes in canonical path order.
    #[must_use]
    pub fn file_changes(&self) -> &[FileChange] {
        &self.file_changes
    }

    /// Consumes the report and returns changes in canonical path order.
    #[must_use]
    pub fn into_file_changes(self) -> Vec<FileChange> {
        self.file_changes
    }
}

impl<'de> Deserialize<'de> for ComparisonResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ComparisonWire {
            left: RunComparisonSummary,
            right: RunComparisonSummary,
            relationship: RunRelationship,
            starting_snapshots: SnapshotComparison,
            final_snapshots: SnapshotComparison,
            file_changes: Vec<FileChange>,
            evaluation: Option<EvaluationComparison>,
        }

        let wire = ComparisonWire::deserialize(deserializer)?;
        Self::from_canonical_parts(
            wire.left,
            wire.right,
            wire.relationship,
            wire.starting_snapshots,
            wire.final_snapshots,
            wire.file_changes,
            wire.evaluation,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// A violated deterministic comparison invariant.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ComparisonValidationError {
    /// A modified record must identify at least one actual difference.
    #[error("modified path contains no change evidence")]
    EmptyModification,
    /// Executable state exists exactly for added or deleted regular files.
    #[error("executable evidence must be present exactly for regular files")]
    InvalidExecutableEvidence,
    /// A path may occur only once in a comparison.
    #[error("duplicate comparison path {path}")]
    DuplicatePath {
        /// The duplicated path.
        path: SnapshotPath,
    },
    /// Persisted comparison changes must already be sorted by path.
    #[error("comparison paths are not in canonical order at index {index}")]
    NonCanonicalOrder {
        /// The first out-of-order entry index.
        index: usize,
    },
    /// Both sides must execute the same nonempty evaluation command.
    #[error("comparison evaluations must use the same nonempty command")]
    InvalidEvaluationCommands,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(run_id: RunId) -> RunComparisonSummary {
        RunComparisonSummary {
            run_id,
            status: RunStatus::Completed,
            exit_status: Some(ProcessExitStatus::Code(0)),
            duration: Some(MonotonicDuration::from_nanoseconds(1)),
            terminal_output_bytes: 10,
            checkpoint_count: 2,
            warning_count: 0,
        }
    }

    fn added(path: &str) -> FileChange {
        FileChange {
            path: path.parse().unwrap(),
            change: FileChangeKind::Added {
                entry_type: SnapshotEntryType::File,
                executable: Some(false),
            },
        }
    }

    #[test]
    fn comparison_change_order_is_deterministic() {
        let left = summary(RunId::generate());
        let right = summary(RunId::generate());
        let snapshots = SnapshotComparison {
            left: Some(SnapshotId::digest(b"left")),
            right: Some(SnapshotId::digest(b"right")),
        };
        let first = ComparisonResult::new(
            left,
            right,
            RunRelationship::Unrelated,
            snapshots,
            snapshots,
            vec![added("z"), added("a")],
            None,
        )
        .unwrap();
        let second = ComparisonResult::new(
            left,
            right,
            RunRelationship::Unrelated,
            snapshots,
            snapshots,
            vec![added("a"), added("z")],
            None,
        )
        .unwrap();
        assert_eq!(first, second);
        let encoded = serde_json::to_vec(&first).unwrap();
        assert_eq!(
            serde_json::from_slice::<ComparisonResult>(&encoded).unwrap(),
            first
        );
    }

    #[test]
    fn empty_modification_is_rejected() {
        let change = FileChangeKind::Modified {
            content_changed: false,
            entry_type: None,
            executable: None,
            permissions: None,
        };
        assert_eq!(
            change.validate(),
            Err(ComparisonValidationError::EmptyModification)
        );
    }
}
