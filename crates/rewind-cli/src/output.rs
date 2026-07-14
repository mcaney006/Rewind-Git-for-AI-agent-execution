use std::io::{self, Write};

use rewind_domain::{
    Checkpoint, ComparisonResult, Event, EventPayload, MonotonicDuration, ProcessExitStatus, Run,
};
use rewind_store::{Store, StoreError, WarningRecord};
use serde::Serialize;
use thiserror::Error;

const SHOW_EVENT_LIMIT: u32 = 1_000;

#[derive(Debug, Error)]
pub(crate) enum OutputError {
    #[error("cannot encode JSON output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot write command output: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(crate) fn print_runs(runs: &[Run], json: bool) -> Result<(), OutputError> {
    if json {
        return write_json(runs);
    }
    let mut stdout = io::stdout().lock();
    if runs.is_empty() {
        writeln!(stdout, "No recorded runs")?;
        return Ok(());
    }
    writeln!(
        stdout,
        "RUN                                   STATUS       DURATION   COMMAND"
    )?;
    for run in runs {
        writeln!(
            stdout,
            "{}  {:<11}  {:<9}  {}{}",
            run.id,
            run.status,
            run.monotonic_duration
                .map_or_else(|| "-".to_owned(), duration),
            run.command,
            format_arguments(&run.arguments),
        )?;
    }
    Ok(())
}

pub(crate) fn print_run(store: &Store, run: &Run, json: bool) -> Result<(), OutputError> {
    let checkpoints = store.load_checkpoints(run.id)?;
    let warnings = store.load_warnings(run.id)?;
    let page = store.load_timeline(run.id, None, SHOW_EVENT_LIMIT)?;
    if json {
        return write_json(&RunView {
            run,
            checkpoints: &checkpoints,
            events: &page.events,
            warnings: warnings.iter().map(WarningView::from).collect(),
            timeline_truncated: page.has_more,
        });
    }
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Run          {}", run.id)?;
    writeln!(stdout, "Status       {}", run.status)?;
    writeln!(
        stdout,
        "Command      {}{}",
        run.command,
        format_arguments(&run.arguments)
    )?;
    writeln!(stdout, "Workspace    {}", run.workspace_root.display())?;
    if let Some(parent) = run.parent {
        writeln!(
            stdout,
            "Parent       {}@{}",
            parent.run_id, parent.checkpoint_id
        )?;
    }
    writeln!(
        stdout,
        "Duration     {}",
        run.monotonic_duration
            .map_or_else(|| "-".to_owned(), duration)
    )?;
    writeln!(
        stdout,
        "Exit         {}",
        run.exit_status.map_or_else(|| "-".to_owned(), exit_status)
    )?;
    writeln!(stdout, "Checkpoints  {}", checkpoints.len())?;
    for checkpoint in &checkpoints {
        writeln!(
            stdout,
            "  {:>6}  {:<22} {}{}",
            checkpoint.sequence,
            checkpoint.reason,
            checkpoint.id,
            checkpoint
                .label
                .as_ref()
                .map_or_else(String::new, |label| format!("  {label:?}")),
        )?;
    }
    writeln!(
        stdout,
        "Events       {}{}",
        page.events.len(),
        if page.has_more { "+" } else { "" }
    )?;
    for event in &page.events {
        writeln!(
            stdout,
            "  {:>6}  {:>10}  {}",
            event.sequence,
            format!("{}ms", event.monotonic_offset.as_nanoseconds() / 1_000_000),
            event_kind(&event.payload)
        )?;
    }
    for warning in warnings {
        writeln!(stdout, "Warning      {}: {}", warning.code, warning.message)?;
    }
    if page.has_more {
        writeln!(
            stdout,
            "Timeline output is bounded to {SHOW_EVENT_LIMIT} events; use replay to navigate the full run."
        )?;
    }
    Ok(())
}

pub(crate) fn print_comparison(
    comparison: &ComparisonResult,
    json: bool,
) -> Result<(), OutputError> {
    if json {
        return write_json(comparison);
    }
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "Runs          {} <> {}",
        comparison.left.run_id, comparison.right.run_id
    )?;
    writeln!(stdout, "Relationship  {:?}", comparison.relationship)?;
    writeln!(
        stdout,
        "Status        {} <> {}",
        comparison.left.status, comparison.right.status
    )?;
    writeln!(
        stdout,
        "Exit          {} <> {}",
        comparison
            .left
            .exit_status
            .map_or_else(|| "-".to_owned(), exit_status),
        comparison
            .right
            .exit_status
            .map_or_else(|| "-".to_owned(), exit_status)
    )?;
    writeln!(
        stdout,
        "Duration      {} <> {}",
        comparison
            .left
            .duration
            .map_or_else(|| "-".to_owned(), duration),
        comparison
            .right
            .duration
            .map_or_else(|| "-".to_owned(), duration)
    )?;
    writeln!(
        stdout,
        "Terminal      {} B <> {} B",
        comparison.left.terminal_output_bytes, comparison.right.terminal_output_bytes
    )?;
    writeln!(
        stdout,
        "Checkpoints   {} <> {}",
        comparison.left.checkpoint_count, comparison.right.checkpoint_count
    )?;
    writeln!(
        stdout,
        "Warnings      {} <> {}",
        comparison.left.warning_count, comparison.right.warning_count
    )?;
    writeln!(stdout, "Changed files {}", comparison.file_changes().len())?;
    for change in comparison.file_changes() {
        writeln!(stdout, "  {:?}  {}", change.change, change.path)?;
    }
    if let Some(evaluation) = &comparison.evaluation {
        writeln!(stdout, "Evaluation    {}", evaluation.left.command)?;
        writeln!(
            stdout,
            "  left        {} in {}",
            exit_status(evaluation.left.exit_status),
            duration(evaluation.left.duration)
        )?;
        writeln!(
            stdout,
            "  right       {} in {}",
            exit_status(evaluation.right.exit_status),
            duration(evaluation.right.duration)
        )?;
    }
    Ok(())
}

pub(crate) fn duration(value: MonotonicDuration) -> String {
    let milliseconds = value.as_nanoseconds() / 1_000_000;
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else {
        format!("{:.2}s", milliseconds as f64 / 1_000.0)
    }
}

pub(crate) fn exit_status(value: ProcessExitStatus) -> String {
    match value {
        ProcessExitStatus::Code(code) => format!("code {code}"),
        ProcessExitStatus::Signal(signal) => format!("signal {signal}"),
        ProcessExitStatus::Unknown => "unknown".to_owned(),
    }
}

pub(crate) fn event_kind(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::RunStarted { .. } => "run started",
        EventPayload::WorkspaceIsolated { .. } => "workspace isolated",
        EventPayload::TerminalInput { .. } => "terminal input",
        EventPayload::TerminalInputRedacted { .. } => "terminal input redacted",
        EventPayload::TerminalOutput { .. } => "terminal output",
        EventPayload::TerminalResized { .. } => "terminal resized",
        EventPayload::ProcessObserved { .. } => "process observed",
        EventPayload::ProcessExited { .. } => "process exited",
        EventPayload::FilesystemPathsDirtied { .. } => "filesystem paths dirtied",
        EventPayload::CheckpointStarted { .. } => "checkpoint started",
        EventPayload::CheckpointCommitted { .. } => "checkpoint committed",
        EventPayload::CheckpointFailed { .. } => "checkpoint failed",
        EventPayload::MarkerCreated { .. } => "marker created",
        EventPayload::RunInterrupted { .. } => "run interrupted",
        EventPayload::RunCompleted { .. } => "run completed",
        EventPayload::RecorderWarning { .. } => "recorder warning",
    }
}

fn format_arguments(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| format!(" {argument:?}"))
        .collect()
}

pub(crate) fn write_json(value: &(impl Serialize + ?Sized)) -> Result<(), OutputError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

#[derive(Serialize)]
struct RunView<'a> {
    run: &'a Run,
    checkpoints: &'a [Checkpoint],
    events: &'a [Event],
    warnings: Vec<WarningView<'a>>,
    timeline_truncated: bool,
}

#[derive(Serialize)]
struct WarningView<'a> {
    sequence: u64,
    code: &'a str,
    message: &'a str,
    created_unix_ms: i64,
}

impl<'a> From<&'a WarningRecord> for WarningView<'a> {
    fn from(value: &'a WarningRecord) -> Self {
        Self {
            sequence: value.sequence,
            code: &value.code,
            message: &value.message,
            created_unix_ms: value.created_at.as_unix_milliseconds(),
        }
    }
}
