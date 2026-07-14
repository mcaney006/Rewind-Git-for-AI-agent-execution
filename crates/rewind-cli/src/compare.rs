use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::Instant;

use rewind_domain::{
    ComparisonResult, EvaluationComparison, EvaluationResult, FileChange, FileChangeKind,
    MonotonicDuration, ProcessExitStatus, Run, RunComparisonSummary, RunId, RunRelationship,
    SnapshotComparison, SnapshotEntry, SnapshotEntryKind, SnapshotEntryType, ValueChange,
};
use rewind_snapshot::{EntryChange, MaterializeOptions, diff_snapshots, materialize};
use rewind_store::{ComparisonInput, Store, StoreError};
use tempfile::TempDir;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CompareError {
    #[error("run {0} has no final snapshot to compare")]
    MissingFinalSnapshot(RunId),
    #[error("cannot create an isolated evaluation directory: {0}")]
    EvaluationDirectory(#[source] std::io::Error),
    #[error("cannot start evaluation command {command:?}: {source}")]
    EvaluationSpawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("comparison evidence violates a domain invariant: {0}")]
    Invalid(#[from] rewind_domain::ComparisonValidationError),
    #[error(transparent)]
    Snapshot(#[from] rewind_snapshot::SnapshotError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(crate) fn compare_runs(
    store: &Store,
    left: RunId,
    right: RunId,
    evaluation: Option<&str>,
    maximum_file_size: u64,
) -> Result<ComparisonResult, CompareError> {
    let left_input = store.load_comparison_input(left)?;
    let right_input = store.load_comparison_input(right)?;
    let runs = store.list_runs()?;
    let relationship = relationship(&left_input.run, &right_input.run, &runs);

    let left_snapshot_id = left_input
        .run
        .final_snapshot
        .ok_or(CompareError::MissingFinalSnapshot(left))?;
    let right_snapshot_id = right_input
        .run
        .final_snapshot
        .ok_or(CompareError::MissingFinalSnapshot(right))?;
    let left_snapshot = store.load_snapshot(left_snapshot_id)?;
    let right_snapshot = store.load_snapshot(right_snapshot_id)?;
    let changes = diff_snapshots(&left_snapshot, &right_snapshot)
        .changes
        .into_iter()
        .map(file_change)
        .collect();
    let evaluation = evaluation
        .map(|command| {
            evaluate_pair(
                store,
                &left_snapshot,
                &right_snapshot,
                command,
                maximum_file_size,
            )
        })
        .transpose()?;

    Ok(ComparisonResult::new(
        summary(&left_input),
        summary(&right_input),
        relationship,
        SnapshotComparison {
            left: left_input.run.initial_snapshot,
            right: right_input.run.initial_snapshot,
        },
        SnapshotComparison {
            left: Some(left_snapshot_id),
            right: Some(right_snapshot_id),
        },
        changes,
        evaluation,
    )?)
}

fn summary(input: &ComparisonInput) -> RunComparisonSummary {
    RunComparisonSummary {
        run_id: input.run.id,
        status: input.run.status,
        exit_status: input.run.exit_status,
        duration: input.run.monotonic_duration,
        terminal_output_bytes: input.terminal_output_bytes,
        checkpoint_count: input.checkpoint_count,
        warning_count: input.warning_count,
    }
}

fn relationship(left: &Run, right: &Run, runs: &[Run]) -> RunRelationship {
    if left.id == right.id {
        return RunRelationship::SameRun;
    }
    if let Some(parent) = right.parent.filter(|parent| parent.run_id == left.id) {
        return RunRelationship::LeftParentOfRight {
            fork_checkpoint: parent.checkpoint_id,
        };
    }
    if let Some(parent) = left.parent.filter(|parent| parent.run_id == right.id) {
        return RunRelationship::RightParentOfLeft {
            fork_checkpoint: parent.checkpoint_id,
        };
    }
    let parents: BTreeMap<_, _> = runs
        .iter()
        .map(|run| (run.id, run.parent.map(|parent| parent.run_id)))
        .collect();
    let left_ancestors = ancestors(left.id, &parents);
    let right_ancestors: BTreeSet<_> = ancestors(right.id, &parents).into_iter().collect();
    left_ancestors
        .into_iter()
        .find(|id| right_ancestors.contains(id))
        .map_or(RunRelationship::Unrelated, |run_id| {
            RunRelationship::CommonAncestor { run_id }
        })
}

fn ancestors(run: RunId, parents: &BTreeMap<RunId, Option<RunId>>) -> Vec<RunId> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = Some(run);
    while let Some(id) = current.filter(|id| seen.insert(*id)) {
        result.push(id);
        current = parents.get(&id).copied().flatten();
    }
    result
}

fn file_change(change: EntryChange) -> FileChange {
    match change {
        EntryChange::Added { entry } => {
            let entry_type = entry_type(&entry.kind);
            let executable = executable(&entry.kind);
            FileChange {
                path: entry.path,
                change: FileChangeKind::Added {
                    entry_type,
                    executable,
                },
            }
        }
        EntryChange::Removed { entry } => {
            let entry_type = entry_type(&entry.kind);
            let executable = executable(&entry.kind);
            FileChange {
                path: entry.path,
                change: FileChangeKind::Deleted {
                    entry_type,
                    executable,
                },
            }
        }
        EntryChange::Modified { before, after } => modified(before, after),
    }
}

fn modified(before: SnapshotEntry, after: SnapshotEntry) -> FileChange {
    let before_type = entry_type(&before.kind);
    let after_type = entry_type(&after.kind);
    let content_changed = content_identity(&before.kind) != content_identity(&after.kind);
    let executable = match (executable(&before.kind), executable(&after.kind)) {
        (Some(left), Some(right)) if left != right => Some(ValueChange { left, right }),
        _ => None,
    };
    let permissions = (before.permissions != after.permissions).then_some(ValueChange {
        left: before.permissions,
        right: after.permissions,
    });
    FileChange {
        path: before.path,
        change: FileChangeKind::Modified {
            content_changed,
            entry_type: (before_type != after_type).then_some(ValueChange {
                left: before_type,
                right: after_type,
            }),
            executable,
            permissions,
        },
    }
}

fn entry_type(kind: &SnapshotEntryKind) -> SnapshotEntryType {
    match kind {
        SnapshotEntryKind::Directory => SnapshotEntryType::Directory,
        SnapshotEntryKind::File { .. } => SnapshotEntryType::File,
        SnapshotEntryKind::Symlink { .. } => SnapshotEntryType::Symlink,
    }
}

fn executable(kind: &SnapshotEntryKind) -> Option<bool> {
    match kind {
        SnapshotEntryKind::File { executable, .. } => Some(*executable),
        SnapshotEntryKind::Directory | SnapshotEntryKind::Symlink { .. } => None,
    }
}

fn content_identity(kind: &SnapshotEntryKind) -> String {
    match kind {
        SnapshotEntryKind::Directory => "directory".to_owned(),
        SnapshotEntryKind::File {
            object_id, size, ..
        } => format!("file:{object_id}:{size}"),
        SnapshotEntryKind::Symlink { target } => format!("symlink:{target}"),
    }
}

fn evaluate_pair(
    store: &Store,
    left: &rewind_domain::Snapshot,
    right: &rewind_domain::Snapshot,
    command: &str,
    maximum_file_size: u64,
) -> Result<EvaluationComparison, CompareError> {
    Ok(EvaluationComparison {
        left: evaluate(store, left, command, maximum_file_size)?,
        right: evaluate(store, right, command, maximum_file_size)?,
    })
}

fn evaluate(
    store: &Store,
    snapshot: &rewind_domain::Snapshot,
    command: &str,
    maximum_file_size: u64,
) -> Result<EvaluationResult, CompareError> {
    let temporary = TempDir::new().map_err(CompareError::EvaluationDirectory)?;
    let workspace = temporary.path().join("workspace");
    materialize(
        snapshot,
        store,
        &workspace,
        &MaterializeOptions {
            force: false,
            max_file_size: maximum_file_size,
        },
    )?;
    let started = Instant::now();
    let status = Command::new("sh")
        .args(["-c", command])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| CompareError::EvaluationSpawn {
            command: command.to_owned(),
            source,
        })?;
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let exit_status = status
        .code()
        .map(ProcessExitStatus::Code)
        .or_else(|| status.signal().map(ProcessExitStatus::Signal))
        .unwrap_or(ProcessExitStatus::Unknown);
    Ok(EvaluationResult {
        command: command.to_owned(),
        exit_status,
        duration: MonotonicDuration::from_nanoseconds(elapsed),
    })
}
