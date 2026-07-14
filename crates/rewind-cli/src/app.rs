use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rewind_capture::{
    CaptureError, CaptureOptions, CaptureOutcome, CaptureRequest, ControlClientError, capture,
    capture_with_store, control_socket_path, request_marker,
};
use rewind_domain::{InputRecordingPolicy, ProcessExitStatus, RunParent};
use rewind_platform::{ApplicationPaths, PathConventionError, application_paths};
use rewind_snapshot::{
    IgnorePattern, MaterializeOptions, ScanOptions, SnapshotError, diff_snapshots, materialize,
};
use rewind_store::{Store, StoreError};
use tempfile::Builder;
use thiserror::Error;

use crate::args::{Command, ExportFormatArg, RecordInputArg};
use crate::artifacts::{self, ArtifactError};
use crate::compare::{CompareError, compare_runs};
use crate::config::{BinaryFileBehavior, Config, ConfigError};
use crate::doctor::{self, DoctorError};
use crate::export::{self, ExportError};
use crate::gc;
use crate::output::{self, OutputError};
use crate::replay::{self, ReplayError};
use crate::resolve::{ResolveError, resolve_checkpoint, resolve_run, resolve_selector};

#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error("cannot resolve current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error("workspace {path} is invalid: {source}")]
    Workspace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "Rewind storage {store} is inside source workspace {workspace}; set REWIND_HOME outside the captured tree"
    )]
    RecursiveStorage { workspace: PathBuf, store: PathBuf },
    #[error("no runs have been recorded; start one with `rewind run -- <command>`")]
    NoRuns,
    #[error("workspace.binary_files=exclude is not yet supported; use explicit ignore patterns")]
    BinaryExclusionUnsupported,
    #[error(
        "privacy.capture_environment=true is not supported by the durable schema; leave it false to avoid implying environment capture"
    )]
    EnvironmentCaptureUnsupported,
    #[error("configured terminal chunk size does not fit this platform")]
    TerminalChunkRange,
    #[error(transparent)]
    Paths(#[from] PathConventionError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Control(#[from] ControlClientError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Output(#[from] OutputError),
    #[error(transparent)]
    Compare(#[from] CompareError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error(transparent)]
    Doctor(#[from] DoctorError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error("cannot create temporary fork workspace in {path}: {source}")]
    ForkTemporary {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot locate the running rewind executable: {0}")]
    CurrentExecutable(#[source] io::Error),
}

pub(crate) fn execute(command: Command) -> Result<u8, AppError> {
    match command {
        Command::Completions { shell, output } => {
            for path in artifacts::completions(shell, &output)? {
                println!("{}", path.display());
            }
            Ok(0)
        }
        Command::Man { output } => {
            artifacts::man_page(&output)?;
            println!("{}", output.display());
            Ok(0)
        }
        command => execute_with_paths(command, application_paths()?),
    }
}

fn execute_with_paths(command: Command, paths: ApplicationPaths) -> Result<u8, AppError> {
    match command {
        Command::Init => {
            let store = Store::open(&paths.data_home)?;
            println!("Initialized Rewind store at {}", store.root().display());
            Ok(0)
        }
        Command::Run {
            workspace,
            record_input,
            id_only,
            command,
        } => run_command(&paths, workspace, record_input, id_only, command),
        Command::List { json } => {
            let store = Store::open_read_only(&paths.data_home)?;
            output::print_runs(&store.list_runs()?, json)?;
            Ok(0)
        }
        Command::Show {
            run,
            checkpoint,
            id_only,
            json,
        } => show(&paths, &run, checkpoint.as_deref(), id_only, json),
        Command::Replay { run } => replay_run(&paths, run.as_deref()),
        Command::Mark { label } => {
            let outcome = request_marker(control_socket_path(&paths.data_home), label)?;
            println!("Marked {}@{}", outcome.run_id, outcome.checkpoint_id);
            Ok(0)
        }
        Command::Checkout {
            selector,
            to,
            force,
        } => checkout(&paths, &selector, &to, force),
        Command::Fork {
            selector,
            record_input,
            id_only,
            command,
        } => fork(&paths, &selector, record_input, id_only, command),
        Command::Compare {
            run_a,
            run_b,
            test,
            json,
        } => compare(&paths, &run_a, &run_b, test.as_deref(), json),
        Command::Export {
            run,
            format,
            output,
        } => export_run(&paths, &run, format, output),
        Command::Doctor { json } => doctor(&paths, json),
        Command::Gc { delete, json } => collect_garbage(&paths, delete, json),
        Command::Completions { .. } | Command::Man { .. } => {
            unreachable!("artifact commands return before path resolution")
        }
    }
}

fn run_command(
    paths: &ApplicationPaths,
    workspace: Option<PathBuf>,
    record_input: Option<RecordInputArg>,
    id_only: bool,
    command: Vec<OsString>,
) -> Result<u8, AppError> {
    let source = canonical_workspace(workspace)?;
    reject_recursive_storage(&source, &paths.data_home)?;
    let config = Config::load(&source, &paths.user_config)?;
    let request = capture_request(&source, record_input, command, &config, None)?;
    if !id_only {
        println!("Recording command in an isolated workspace");
        println!("Source       {}", source.display());
    }
    let outcome = capture(&paths.data_home, request)?;
    report_capture(&paths.data_home, &outcome, id_only)
}

fn fork(
    paths: &ApplicationPaths,
    selector: &str,
    record_input: Option<RecordInputArg>,
    id_only: bool,
    command: Vec<OsString>,
) -> Result<u8, AppError> {
    let mut store = Store::open(&paths.data_home)?;
    let (parent_run, checkpoint) = resolve_selector(&store, selector)?;
    let snapshot = store.load_snapshot(checkpoint.snapshot_id)?;
    let initial_config = load_config_for_current_directory(paths)?;
    let temporary = Builder::new()
        .prefix("fork-")
        .tempdir_in(&paths.data_home)
        .map_err(|source| AppError::ForkTemporary {
            path: paths.data_home.clone(),
            source,
        })?;
    let source = temporary.path().join("source");
    materialize(
        &snapshot,
        &store,
        &source,
        &MaterializeOptions {
            force: false,
            max_file_size: initial_config.workspace.max_file_size.bytes(),
        },
    )?;
    let config = Config::load(&source, &paths.user_config)?;
    let request = capture_request(
        &source,
        record_input,
        command,
        &config,
        Some(RunParent {
            run_id: parent_run.id,
            checkpoint_id: checkpoint.id,
        }),
    )?;
    if !id_only {
        println!("Forking from {}@{}", parent_run.id, checkpoint.id);
    }
    let outcome = capture_with_store(&mut store, request)?;
    drop(store);
    report_capture(&paths.data_home, &outcome, id_only)
}

fn capture_request(
    source: &Path,
    record_input: Option<RecordInputArg>,
    command: Vec<OsString>,
    config: &Config,
    parent: Option<RunParent>,
) -> Result<CaptureRequest, AppError> {
    let (program, arguments) = command.split_first().ok_or(CaptureError::InvalidCommand)?;
    let mut request = CaptureRequest::new(source, program.clone());
    request.arguments = arguments.to_vec();
    request.parent = parent;
    request.rewind_binary = Some(std::env::current_exe().map_err(AppError::CurrentExecutable)?);
    request.options = capture_options(config, record_input)?;
    Ok(request)
}

fn capture_options(
    config: &Config,
    record_input: Option<RecordInputArg>,
) -> Result<CaptureOptions, AppError> {
    if config.workspace.binary_files == BinaryFileBehavior::Exclude {
        return Err(AppError::BinaryExclusionUnsupported);
    }
    if config.privacy.capture_environment {
        return Err(AppError::EnvironmentCaptureUnsupported);
    }
    let patterns: BTreeSet<_> = config
        .workspace
        .ignore
        .iter()
        .chain(&config.privacy.excluded_paths)
        .cloned()
        .collect();
    let ignore = patterns
        .into_iter()
        .map(|pattern| pattern.parse::<IgnorePattern>())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CaptureOptions {
        record_input: record_input
            .map(InputRecordingPolicy::from)
            .unwrap_or(config.capture.record_input),
        capture_environment: false,
        terminal_chunk_size: usize::try_from(config.capture.terminal_chunk_size.bytes())
            .map_err(|_| AppError::TerminalChunkRange)?,
        terminal_max_bytes: config.capture.terminal_max_bytes.bytes(),
        max_run_bytes: config.storage.max_run_size.bytes(),
        process_poll_interval: config.capture.process_poll_interval.duration(),
        checkpoint_debounce: config.capture.checkpoint_debounce.duration(),
        checkpoint_min_interval: config.capture.checkpoint_min_interval.duration(),
        checkpoint_max_interval: config.capture.checkpoint_max_interval.duration(),
        maximum_pending_dirty_paths: config.capture.maximum_pending_dirty_paths,
        snapshot: ScanOptions {
            ignore,
            max_file_size: config.workspace.max_file_size.bytes(),
        },
        ..CaptureOptions::default()
    })
}

fn report_capture(
    store_root: &Path,
    outcome: &CaptureOutcome,
    id_only: bool,
) -> Result<u8, AppError> {
    if id_only {
        println!("{}", outcome.run_id);
        return Ok(process_exit_code(outcome.exit_status));
    }
    let store = Store::open_read_only(store_root)?;
    let changed = diff_snapshots(
        &store.load_snapshot(outcome.initial_snapshot)?,
        &store.load_snapshot(outcome.final_snapshot)?,
    )
    .changes
    .len();
    println!();
    println!("Run {} {}", outcome.status, outcome.run_id);
    println!("Workspace     {}", outcome.workspace_root.display());
    println!("Clone         {:?}", outcome.clone_strategy);
    println!("Exit          {}", output::exit_status(outcome.exit_status));
    println!("Checkpoints   {}", outcome.checkpoint_count);
    println!("Changed       {changed} paths");
    println!("Replay        rewind replay {}", outcome.run_id);
    println!(
        "Fork          rewind fork {}@final -- <command>",
        outcome.run_id
    );
    Ok(process_exit_code(outcome.exit_status))
}

fn show(
    paths: &ApplicationPaths,
    run_value: &str,
    checkpoint_value: Option<&str>,
    id_only: bool,
    json: bool,
) -> Result<u8, AppError> {
    let store = Store::open_read_only(&paths.data_home)?;
    let run = resolve_run(&store, run_value)?;
    if let Some(value) = checkpoint_value {
        let checkpoint = resolve_checkpoint(&store, &run, value)?;
        if id_only {
            println!("{}", checkpoint.id);
        } else if json {
            output::write_json(&checkpoint)?;
        } else {
            output::print_run(&store, &run, false)?;
            println!("Selected     {} ({})", checkpoint.id, checkpoint.reason);
            println!("Snapshot     {}", checkpoint.snapshot_id);
        }
    } else {
        output::print_run(&store, &run, json)?;
    }
    Ok(0)
}

fn replay_run(paths: &ApplicationPaths, run_value: Option<&str>) -> Result<u8, AppError> {
    let store = Store::open_read_only(&paths.data_home)?;
    let run = match run_value {
        Some(value) => resolve_run(&store, value)?,
        None => store
            .list_runs()?
            .into_iter()
            .next()
            .ok_or(AppError::NoRuns)?,
    };
    let config = load_config_for_current_directory(paths)?;
    replay::replay(
        &paths.data_home,
        &store,
        run.id,
        config.replay.terminal_cache.bytes(),
    )?;
    Ok(0)
}

fn checkout(
    paths: &ApplicationPaths,
    selector: &str,
    destination: &Path,
    force: bool,
) -> Result<u8, AppError> {
    let store = Store::open_read_only(&paths.data_home)?;
    let (_, checkpoint) = resolve_selector(&store, selector)?;
    let config = load_config_for_current_directory(paths)?;
    let report = materialize(
        &store.load_snapshot(checkpoint.snapshot_id)?,
        &store,
        destination,
        &MaterializeOptions {
            force,
            max_file_size: config.workspace.max_file_size.bytes(),
        },
    )?;
    println!(
        "Checked out {} to {}",
        checkpoint.id,
        report.destination.display()
    );
    println!(
        "Files {}  directories {}  symlinks {}  bytes {}",
        report.files, report.directories, report.symlinks, report.logical_bytes
    );
    for warning in report.warnings {
        eprintln!("warning: {warning:?}");
    }
    Ok(0)
}

fn compare(
    paths: &ApplicationPaths,
    left: &str,
    right: &str,
    test: Option<&str>,
    json: bool,
) -> Result<u8, AppError> {
    let store = Store::open_read_only(&paths.data_home)?;
    let left = resolve_run(&store, left)?;
    let right = resolve_run(&store, right)?;
    let config = load_config_for_current_directory(paths)?;
    let comparison = compare_runs(
        &store,
        left.id,
        right.id,
        test,
        config.workspace.max_file_size.bytes(),
    )?;
    output::print_comparison(&comparison, json)?;
    Ok(0)
}

fn export_run(
    paths: &ApplicationPaths,
    run_value: &str,
    format: ExportFormatArg,
    output_path: Option<PathBuf>,
) -> Result<u8, AppError> {
    let store = Store::open_read_only(&paths.data_home)?;
    let run = resolve_run(&store, run_value)?;
    let config = load_config_for_current_directory(paths)?;
    match format {
        ExportFormatArg::Html => {
            let path =
                export::export_html(&store, &run, output_path, config.privacy.redact_exports)?;
            println!("Exported offline replay to {}", path.display());
        }
        ExportFormatArg::Bundle => {
            let report = export::export_bundle(
                &store,
                &run,
                output_path,
                config.privacy.redact_exports,
                config.storage.max_run_size.bytes(),
            )?;
            println!("Exported bundle to {}", report.path.display());
            println!(
                "Entries {}  payload bytes {}  redacted input events {}",
                report.entries, report.payload_bytes, report.redacted_input_events
            );
        }
    }
    Ok(0)
}

fn doctor(paths: &ApplicationPaths, json: bool) -> Result<u8, AppError> {
    let report = doctor::inspect(&paths.data_home)?;
    if json {
        output::write_json(&report)?;
    } else {
        println!(
            "Rewind doctor: {}",
            if report.healthy { "healthy" } else { "errors" }
        );
        for check in &report.checks {
            println!("  {:<20} {:?}: {}", check.name, check.status, check.detail);
        }
    }
    Ok(if report.healthy { 0 } else { 2 })
}

fn collect_garbage(paths: &ApplicationPaths, delete: bool, json: bool) -> Result<u8, AppError> {
    let report = gc::collect(&paths.data_home, delete)?;
    if json {
        output::write_json(&report)?;
    } else {
        println!(
            "{} unreachable object(s), {} reclaimable bytes",
            report.unreachable_objects, report.reclaimable_bytes
        );
        println!(
            "{} unindexed/temp crash artifact(s), {} reclaimable bytes",
            report.crash_artifact_files, report.crash_artifact_bytes
        );
        if delete {
            println!(
                "Deleted {} object(s), reclaimed {} bytes",
                report.deleted_objects, report.reclaimed_bytes
            );
            println!(
                "Deleted {} crash artifact(s), reclaimed {} bytes",
                report.deleted_crash_artifact_files, report.reclaimed_crash_artifact_bytes
            );
        } else if report.unreachable_objects != 0 || report.crash_artifact_files != 0 {
            println!("Dry run only; pass --delete to remove unreachable objects and artifacts.");
        }
        for id in &report.sample {
            println!("  {id}");
        }
        if report.sample_truncated {
            println!("  ... sample limited to {} objects", report.sample.len());
        }
        for path in &report.crash_artifact_sample {
            println!("  {path}");
        }
        if report.crash_artifact_sample_truncated {
            println!(
                "  ... sample limited to {} crash artifacts",
                report.crash_artifact_sample.len()
            );
        }
    }
    Ok(0)
}

fn canonical_workspace(workspace: Option<PathBuf>) -> Result<PathBuf, AppError> {
    let path = match workspace {
        Some(path) => path,
        None => std::env::current_dir().map_err(AppError::CurrentDirectory)?,
    };
    let canonical = fs::canonicalize(&path).map_err(|source| AppError::Workspace {
        path: path.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(AppError::Workspace {
            path: canonical,
            source: io::Error::new(io::ErrorKind::InvalidInput, "workspace is not a directory"),
        });
    }
    Ok(canonical)
}

fn reject_recursive_storage(workspace: &Path, store: &Path) -> Result<(), AppError> {
    let store = resolve_future_path(store).map_err(|source| AppError::Workspace {
        path: store.to_path_buf(),
        source,
    })?;
    if store == workspace || store.starts_with(workspace) {
        Err(AppError::RecursiveStorage {
            workspace: workspace.to_path_buf(),
            store,
        })
    } else {
        Ok(())
    }
}

fn resolve_future_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.try_exists()? {
        let name = existing.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no existing ancestor")
        })?;
        missing.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no existing ancestor")
        })?;
    }
    let mut resolved = fs::canonicalize(existing)?;
    for component in missing.into_iter().rev() {
        if component == std::ffi::OsStr::new("..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "storage path contains parent traversal",
            ));
        }
        resolved.push(component);
    }
    Ok(resolved)
}

fn load_config_for_current_directory(paths: &ApplicationPaths) -> Result<Config, AppError> {
    let current = std::env::current_dir().map_err(AppError::CurrentDirectory)?;
    Ok(Config::load(&current, &paths.user_config)?)
}

fn process_exit_code(status: ProcessExitStatus) -> u8 {
    match status {
        ProcessExitStatus::Code(code) => {
            u8::try_from(code).unwrap_or(if code < 0 { 1 } else { 255 })
        }
        ProcessExitStatus::Signal(signal) => {
            u8::try_from(128_i32.saturating_add(signal)).unwrap_or(1)
        }
        ProcessExitStatus::Unknown => 1,
    }
}
