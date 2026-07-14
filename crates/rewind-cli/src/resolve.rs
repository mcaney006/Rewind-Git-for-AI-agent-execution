use rewind_domain::{Checkpoint, CheckpointReason, Run, RunId};
use rewind_store::{Store, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ResolveError {
    #[error("selector must use <run>@<checkpoint>")]
    InvalidSelector,
    #[error("run {0:?} was not found")]
    RunNotFound(String),
    #[error("run prefix {prefix:?} is ambiguous; matches: {matches}")]
    AmbiguousRun { prefix: String, matches: String },
    #[error("checkpoint {checkpoint:?} was not found in run {run}")]
    CheckpointNotFound { run: RunId, checkpoint: String },
    #[error("checkpoint selector {checkpoint:?} is ambiguous in run {run}; matches: {matches}")]
    AmbiguousCheckpoint {
        run: RunId,
        checkpoint: String,
        matches: String,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(crate) fn resolve_run(store: &Store, value: &str) -> Result<Run, ResolveError> {
    if value.is_empty() {
        return Err(ResolveError::RunNotFound(value.to_owned()));
    }
    let runs = store.list_runs()?;
    let mut matches = runs
        .into_iter()
        .filter(|run| run.id.to_string().starts_with(value));
    let Some(run) = matches.next() else {
        return Err(ResolveError::RunNotFound(value.to_owned()));
    };
    if let Some(second) = matches.next() {
        return Err(ResolveError::AmbiguousRun {
            prefix: value.to_owned(),
            matches: format!("{}, {}", run.id, second.id),
        });
    }
    Ok(run)
}

pub(crate) fn resolve_selector(
    store: &Store,
    selector: &str,
) -> Result<(Run, Checkpoint), ResolveError> {
    let (run, checkpoint) = selector
        .split_once('@')
        .filter(|(run, checkpoint)| {
            !run.is_empty() && !checkpoint.is_empty() && !checkpoint.contains('@')
        })
        .ok_or(ResolveError::InvalidSelector)?;
    let run = resolve_run(store, run)?;
    let checkpoint = resolve_checkpoint(store, &run, checkpoint)?;
    Ok((run, checkpoint))
}

pub(crate) fn resolve_checkpoint(
    store: &Store,
    run: &Run,
    value: &str,
) -> Result<Checkpoint, ResolveError> {
    let checkpoints = store.load_checkpoints(run.id)?;
    let mut matches: Vec<_> = checkpoints
        .iter()
        .filter(|checkpoint| checkpoint_matches(checkpoint, value))
        .cloned()
        .collect();
    if value == "initial" || value == "final" {
        let reason = if value == "initial" {
            CheckpointReason::Initial
        } else {
            CheckpointReason::Final
        };
        matches = checkpoints
            .into_iter()
            .filter(|checkpoint| checkpoint.reason == reason)
            .collect();
    }
    match matches.as_slice() {
        [] => Err(ResolveError::CheckpointNotFound {
            run: run.id,
            checkpoint: value.to_owned(),
        }),
        [checkpoint] => Ok(checkpoint.clone()),
        _ => Err(ResolveError::AmbiguousCheckpoint {
            run: run.id,
            checkpoint: value.to_owned(),
            matches: matches
                .iter()
                .take(4)
                .map(|checkpoint| checkpoint.id.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

fn checkpoint_matches(checkpoint: &&Checkpoint, value: &str) -> bool {
    checkpoint.id.to_string().starts_with(value) || checkpoint.label.as_deref() == Some(value)
}
