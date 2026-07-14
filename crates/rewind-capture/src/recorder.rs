use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rewind_domain::{
    BranchId, Checkpoint, CheckpointId, CheckpointReason, Event, EventPayload, EventSequence,
    InputRecordingPolicy, InputRedactionReason, MAX_DIRTY_PATHS_PER_EVENT, MAX_EVENT_TEXT_BYTES,
    MonotonicDuration, ObjectId, Platform, ProcessExitStatus, ProcessId, ProcessObservation,
    ProcessRelationship, RecorderFailure, RecorderFailureKind, RecorderWarning,
    RecorderWarningCode, Run, RunId, RunStatus, SnapshotEntryKind, SnapshotId, SnapshotPath,
    TerminalStreamId, Timestamp, WorkspaceCloneStrategy,
};
use rewind_platform::{
    ChildExit, CloneReport, CloneStrategy, DirectoryEntryKind, DirectoryRoot, PinnedDirectory,
    ProcessInfo, PtyChild, PtyEchoProbe, PtyMaster, PtyProcess, TerminalModeGuard, clone_workspace,
    create_private_dir, remove_relative_entry, spawn_pty, supervised_processes, terminal_size,
};
use rewind_snapshot::{ScanReport, SnapshotError, scan_workspace};
use rewind_store::{RunFinish, Store};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGWINCH};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use crate::control::ControlServer;
use crate::{CaptureError, CaptureOptions, CaptureOutcome, CaptureRequest};

const COORDINATOR_TICK: Duration = Duration::from_millis(20);
const SIGNAL_GRACE_PERIOD: Duration = Duration::from_secs(2);
const POST_ROOT_DRAIN_GRACE: Duration = Duration::from_secs(2);
const POST_KILL_DRAIN_GRACE: Duration = Duration::from_secs(1);
const PRODUCER_RETRY: Duration = Duration::from_millis(5);
const MAX_DIRTY_SCAN_ENTRIES: usize = 1_000_000;

pub(crate) enum Observation {
    TerminalOutput(Vec<u8>),
    TerminalInput {
        bytes: Vec<u8>,
        echo_enabled: Option<bool>,
    },
    OutputEof,
    ProcessSnapshot(Result<Vec<ProcessInfo>, String>),
    Signal(i32),
    Marker {
        label: Option<String>,
        reply: SyncSender<Result<CheckpointId, String>>,
    },
    ProducerFailed {
        producer: &'static str,
        message: String,
    },
}

pub(crate) fn send_until_stopped(
    sender: &SyncSender<Observation>,
    mut observation: Observation,
    stop: &AtomicBool,
) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(observation) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                observation = returned;
                thread::sleep(PRODUCER_RETRY);
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

pub(crate) fn capture_with_store(
    store: &mut Store,
    request: CaptureRequest,
) -> Result<CaptureOutcome, CaptureError> {
    request.options.validate()?;
    let command = request
        .command
        .to_str()
        .filter(|value| !value.is_empty())
        .ok_or(CaptureError::InvalidCommand)?
        .to_owned();
    let arguments = request
        .arguments
        .iter()
        .enumerate()
        .map(|(index, argument)| {
            argument
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or(CaptureError::InvalidArgument { index })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let source_workspace =
        canonicalize("canonicalize source workspace", &request.source_workspace)?;
    let store_root = canonicalize("canonicalize store", store.root())?;
    if store_root == source_workspace || store_root.starts_with(&source_workspace) {
        return Err(CaptureError::RecursiveStore {
            store: store_root,
            workspace: source_workspace,
        });
    }
    let run_id = RunId::generate();
    let run_directory = store_root.join("runs").join(run_id.to_string());
    if run_directory.exists() {
        return Err(CaptureError::Io {
            operation: "create unique run directory",
            path: run_directory,
            source: io::Error::new(io::ErrorKind::AlreadyExists, "generated run path exists"),
        });
    }
    create_private_dir(&run_directory)?;
    let workspace_root = run_directory.join("workspace");
    let started_at = timestamp_now()?;
    let started = Instant::now();
    let run = Run {
        id: run_id,
        branch_id: request.branch_id.unwrap_or_else(BranchId::generate),
        parent: request.parent,
        command: command.clone(),
        arguments,
        workspace_root: workspace_root.clone(),
        started_at,
        finished_at: None,
        monotonic_duration: None,
        status: RunStatus::Preparing,
        platform: current_platform()?,
        capture_policy: request.options.policy(),
        initial_snapshot: None,
        final_snapshot: None,
        exit_status: None,
    };
    store.create_run(&run)?;

    let stream_id = TerminalStreamId::generate();
    let mut recorder = Recorder::new(
        store,
        run_id,
        stream_id,
        workspace_root.clone(),
        request.options,
        started,
    );
    let clone_report = match clone_workspace(&source_workspace, &workspace_root) {
        Ok(report) => report,
        Err(error) => {
            recorder.finish_preparation_failure(&error.to_string());
            return Err(error.into());
        }
    };

    let initial = match recorder.checkpoint(CheckpointReason::Initial, None) {
        Ok(commit) => commit,
        Err(error) => {
            recorder.finish_preparation_failure(&error.to_string());
            return Err(error);
        }
    };
    if let Err(error) = recorder.store.mark_run_running(run_id, initial.snapshot_id) {
        recorder.finish_preparation_failure(&error.to_string());
        return Err(error.into());
    }
    let dirty = match DirtyTracker::new(&workspace_root, &recorder.options) {
        Ok(tracker) => tracker,
        Err(error) => {
            recorder.finish_running_without_child(initial.snapshot_id, &error.to_string());
            return Err(error);
        }
    };

    let stdin = io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    let interactive_stdin = stdin.is_terminal();
    let mut terminal_size_uncertain = false;
    let initial_size = if interactive_stdin {
        match terminal_size(stdin_fd) {
            Ok(size) => size,
            Err(_) => {
                terminal_size_uncertain = true;
                recorder.options.fallback_terminal_size
            }
        }
    } else {
        recorder.options.fallback_terminal_size
    };
    let mut terminal_guard = if interactive_stdin {
        match TerminalModeGuard::enter_raw(stdin_fd) {
            Ok(guard) => Some(guard),
            Err(error) => {
                recorder.finish_running_without_child(initial.snapshot_id, &error.to_string());
                return Err(error.into());
            }
        }
    } else {
        None
    };

    let (sender, receiver) = sync_channel(recorder.options.channel_capacity);
    let mut control = match ControlServer::start(&store_root, run_id, sender.clone()) {
        Ok(server) => server,
        Err(message) => {
            recorder.finish_running_without_child(initial.snapshot_id, &message);
            return restore_parent_terminal(
                &mut terminal_guard,
                Err(CaptureError::Control(message)),
            );
        }
    };
    let mut environment = request.extra_environment;
    set_environment(
        &mut environment,
        "REWIND_CONTROL_SOCKET",
        crate::control_socket_path(&store_root).into_os_string(),
    );
    set_environment(&mut environment, "REWIND_RUN_ID", run_id.to_string());
    set_environment(&mut environment, "REWIND_HOME", store_root.as_os_str());
    if let Some(binary) = request.rewind_binary {
        set_environment(&mut environment, "REWIND_BIN", binary.as_os_str());
    }

    let mut process = match spawn_pty(
        &request.command,
        &request.arguments,
        &workspace_root,
        &environment,
        initial_size,
    ) {
        Ok(process) => process,
        Err(error) => {
            let _ = control.stop();
            recorder.finish_running_without_child(initial.snapshot_id, &error.to_string());
            return restore_parent_terminal(&mut terminal_guard, Err(error.into()));
        }
    };
    if !interactive_stdin && let Err(error) = process.master.set_echo_enabled(false) {
        if process.child.kill().is_ok() {
            let _ = process.child.wait();
        }
        let _ = control.stop();
        recorder.finish_running_without_child(initial.snapshot_id, &error.to_string());
        return restore_parent_terminal(&mut terminal_guard, Err(error.into()));
    }
    let root_pid = match process.child.process_id().and_then(ProcessId::new) {
        Some(process_id) => process_id,
        None => {
            let mut child = process.child;
            if child.kill().is_ok() {
                let _ = child.wait();
            }
            let _ = control.stop();
            recorder.finish_running_without_child(
                initial.snapshot_id,
                "PTY returned no valid process ID",
            );
            return restore_parent_terminal(
                &mut terminal_guard,
                Err(CaptureError::InvalidProcessId),
            );
        }
    };

    let startup = (|| {
        recorder.emit(EventPayload::RunStarted {
            root_process_id: root_pid,
            terminal_stream_id: stream_id,
        })?;
        recorder.emit(EventPayload::WorkspaceIsolated {
            strategy: match clone_report.strategy {
                CloneStrategy::ApfsClone => WorkspaceCloneStrategy::ApfsClone,
                CloneStrategy::LinuxReflink => WorkspaceCloneStrategy::LinuxReflink,
                CloneStrategy::Mixed => WorkspaceCloneStrategy::Mixed,
                CloneStrategy::RecursiveCopy => WorkspaceCloneStrategy::RecursiveCopy,
            },
        })?;
        recorder.emit(EventPayload::TerminalResized {
            stream_id,
            columns: initial_size.columns,
            rows: initial_size.rows,
        })?;
        recorder.emit(EventPayload::ProcessObserved {
            process: ProcessObservation {
                process_id: root_pid,
                parent_process_id: None,
                executable: None,
                command,
                relationship: ProcessRelationship::Root,
            },
        })?;
        match clone_report.strategy {
            CloneStrategy::Mixed => recorder.warning(
                RecorderWarningCode::CloneFallback,
                "Workspace isolation used a mix of copy-on-write clones and byte copies; large workspaces may take longer.",
            )?,
            CloneStrategy::RecursiveCopy => recorder.warning(
                RecorderWarningCode::CloneFallback,
                "Workspace isolation used a recursive byte copy; large workspaces may take longer.",
            )?,
            CloneStrategy::ApfsClone | CloneStrategy::LinuxReflink => {}
        }
        if terminal_size_uncertain {
            recorder.warning(
                RecorderWarningCode::Other,
                "Could not read the parent terminal size; the configured fallback was used.",
            )?;
        }
        recorder.warn_exclusions(initial.exclusions)?;
        recorder.flush()
    })();
    if let Err(error) = startup {
        if process.child.kill().is_ok() {
            let _ = process.child.wait();
        }
        let _ = control.stop();
        recorder.finish_running_without_child(initial.snapshot_id, &error.to_string());
        return restore_parent_terminal(&mut terminal_guard, Err(error));
    }

    let result = supervise(
        &mut recorder,
        Supervision {
            process,
            root_pid,
            stdin_fd,
            interactive_stdin,
            sender,
            receiver,
            initial_snapshot: initial.snapshot_id,
            clone_report,
            dirty,
        },
        &mut control,
    );
    let _ = control.stop();
    if let Err(error) = &result {
        if recorder
            .store
            .load_run(run_id)
            .is_ok_and(|run| run.status == RunStatus::Running)
        {
            recorder.finish_running_without_child(initial.snapshot_id, &error.to_string());
        }
        let _ = recorder.cleanup_exclusions_for_retention();
    }
    restore_parent_terminal(&mut terminal_guard, result)
}

fn restore_parent_terminal<T>(
    guard: &mut Option<TerminalModeGuard>,
    result: Result<T, CaptureError>,
) -> Result<T, CaptureError> {
    let restore = guard.as_mut().map_or(Ok(()), TerminalModeGuard::restore);
    combine_terminal_restore(result, restore)
}

fn combine_terminal_restore<T>(
    result: Result<T, CaptureError>,
    restore: Result<(), rewind_platform::TerminalError>,
) -> Result<T, CaptureError> {
    match (result, restore) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(primary), Err(restore)) => Err(CaptureError::TerminalRestoreAfterFailure {
            primary: Box::new(primary),
            restore,
        }),
    }
}

fn supervise(
    recorder: &mut Recorder<'_>,
    supervision: Supervision,
    control: &mut ControlServer,
) -> Result<CaptureOutcome, CaptureError> {
    let Supervision {
        process,
        root_pid,
        stdin_fd,
        interactive_stdin,
        sender,
        receiver,
        initial_snapshot,
        clone_report,
        dirty,
    } = supervision;
    let PtyProcess {
        master,
        reader,
        writer,
        child,
    } = process;
    let mut child_guard = ChildGuard(Some(child));
    let echo_probe = match recorder.options.record_input {
        InputRecordingPolicy::Auto => master.try_clone_echo_probe().ok(),
        InputRecordingPolicy::Always | InputRecordingPolicy::Never => None,
    };
    let output = spawn_output(reader, sender.clone(), recorder.options.terminal_chunk_size)?;
    spawn_input(
        writer,
        echo_probe,
        sender.clone(),
        recorder.options.terminal_chunk_size,
    )?;
    let mut process_observer = ProcessProducer::start(
        root_pid.get(),
        sender.clone(),
        recorder.options.process_poll_interval,
    )?;
    let mut signals = SignalProducer::start(sender)?;
    let child = child_guard.take();
    let mut coordinator = Coordinator {
        recorder,
        receiver,
        master,
        child,
        root_pid,
        stdin_fd,
        interactive_stdin,
        output_eof: false,
        output_abandoned: false,
        child_exit: None,
        interrupted_signal: None,
        termination_deadline: None,
        post_root_drain_deadline: None,
        post_root_kill_requested: false,
        terminal_output_bytes: 0,
        // The startup event establishes the root immediately. Keep the
        // observation set empty so the first platform sample can enrich that
        // root with its executable and parent metadata when available.
        observed_processes: BTreeSet::new(),
        process_warning_emitted: false,
        echo_warning_emitted: false,
        dirty_warning_emitted: false,
        fatal: None,
        dirty,
    };

    while coordinator.child_exit.is_none()
        || (!coordinator.output_eof && !coordinator.output_abandoned)
    {
        match coordinator.receiver.recv_timeout(COORDINATOR_TICK) {
            Ok(observation) => coordinator.handle(observation),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) if coordinator.output_eof => {}
            Err(RecvTimeoutError::Disconnected) => coordinator.fail(CaptureError::Producer {
                producer: "channel",
                message: "all producer handles disconnected before PTY output ended".to_owned(),
            }),
        }
        coordinator.poll_child();
        coordinator.poll_post_root_drain();
        coordinator.poll_workspace();
        if coordinator.child_exit.is_some() {
            control.request_stop();
            process_observer.request_stop();
        }
    }

    process_observer.request_stop();
    signals.request_stop();
    control.request_stop();
    let process_group_stopped = coordinator.terminate_remaining_process_group();
    drain_ready(&mut coordinator);
    let output_result = if coordinator.output_abandoned {
        // ponytail: a descendant can escape the owned process group while retaining
        // the PTY. Detach its blocked reader after the durable warning; a pollable
        // platform reader is the upgrade if capture becomes a long-lived library.
        drop(output);
        Ok(())
    } else {
        join_output_draining(output, &mut coordinator)
    };
    let process_result = process_observer.stop();
    let signal_result = signals.stop();
    if let Err(error) = output_result {
        coordinator.fail(error);
    }
    if let Err(error) = process_result {
        coordinator.fail(error);
    }
    if let Err(error) = signal_result {
        coordinator.fail(error);
    }

    let exit_status = coordinator
        .child_exit
        .as_ref()
        .map(domain_exit)
        .unwrap_or(ProcessExitStatus::Unknown);
    if !coordinator.fatal.as_ref().is_some_and(is_store_failure)
        && let Err(error) = coordinator.recorder.emit(EventPayload::ProcessExited {
            process_id: root_pid,
            status: exit_status,
        })
    {
        coordinator.fail(error);
    }

    let final_commit =
        if coordinator.fatal.as_ref().is_some_and(is_store_failure) || !process_group_stopped {
            None
        } else {
            match coordinator
                .recorder
                .checkpoint(CheckpointReason::Final, None)
            {
                Ok(commit) => {
                    if let Err(error) = coordinator.recorder.warn_exclusions(commit.exclusions) {
                        coordinator.fail(error);
                    }
                    Some(commit)
                }
                Err(error) => {
                    coordinator.fail(error);
                    None
                }
            }
        };
    let final_snapshot = final_commit.as_ref().map(|commit| commit.snapshot_id);
    if let Err(error) = coordinator.recorder.cleanup_exclusions_for_retention() {
        coordinator.fail(error);
    }
    let status = if coordinator.interrupted_signal.is_some() {
        RunStatus::Interrupted
    } else if coordinator.fatal.is_some() {
        RunStatus::Failed
    } else if exit_status.success() == Some(true) {
        RunStatus::Completed
    } else {
        RunStatus::Failed
    };
    let finish_result = coordinator
        .recorder
        .finish(status, exit_status, final_snapshot);
    if let Err(error) = finish_result {
        coordinator.fail(error);
    }

    if let Some(error) = coordinator.fatal {
        return Err(error);
    }
    let final_snapshot = final_snapshot.ok_or_else(|| CaptureError::Producer {
        producer: "checkpoint",
        message: "final authoritative snapshot was not committed".to_owned(),
    })?;
    Ok(CaptureOutcome {
        run_id: coordinator.recorder.run_id,
        workspace_root: coordinator.recorder.workspace.clone(),
        clone_strategy: clone_report.strategy,
        initial_snapshot,
        final_snapshot,
        status,
        exit_status,
        checkpoint_count: coordinator.recorder.checkpoint_count,
        terminal_output_bytes: coordinator.terminal_output_bytes,
    })
}

struct ChildGuard(Option<PtyChild>);

impl ChildGuard {
    fn take(&mut self) -> PtyChild {
        self.0
            .take()
            .expect("child guard contains the PTY child until coordinator construction")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if child.kill_process_group().is_err() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

struct Supervision {
    process: PtyProcess,
    root_pid: ProcessId,
    stdin_fd: i32,
    interactive_stdin: bool,
    sender: SyncSender<Observation>,
    receiver: Receiver<Observation>,
    initial_snapshot: SnapshotId,
    clone_report: CloneReport,
    dirty: DirtyTracker,
}

struct Coordinator<'recorder, 'store> {
    recorder: &'recorder mut Recorder<'store>,
    receiver: Receiver<Observation>,
    master: PtyMaster,
    child: PtyChild,
    root_pid: ProcessId,
    stdin_fd: i32,
    interactive_stdin: bool,
    output_eof: bool,
    output_abandoned: bool,
    child_exit: Option<ChildExit>,
    interrupted_signal: Option<i32>,
    termination_deadline: Option<Instant>,
    post_root_drain_deadline: Option<Instant>,
    post_root_kill_requested: bool,
    terminal_output_bytes: u64,
    observed_processes: BTreeSet<ProcessId>,
    process_warning_emitted: bool,
    echo_warning_emitted: bool,
    dirty_warning_emitted: bool,
    fatal: Option<CaptureError>,
    dirty: DirtyTracker,
}

impl Coordinator<'_, '_> {
    fn handle(&mut self, observation: Observation) {
        if self.fatal.is_some() {
            match observation {
                Observation::OutputEof => {
                    self.output_eof = true;
                    self.post_root_drain_deadline = None;
                }
                Observation::Marker { reply, .. } => {
                    let _ = reply.send(Err("recorder is stopping after a failure".to_owned()));
                }
                Observation::TerminalOutput(_)
                | Observation::TerminalInput { .. }
                | Observation::ProcessSnapshot(_)
                | Observation::Signal(_)
                | Observation::ProducerFailed { .. } => {}
            }
            return;
        }
        let result = match observation {
            Observation::TerminalOutput(bytes) => self.record_output(&bytes),
            Observation::TerminalInput {
                bytes,
                echo_enabled,
            } => self.record_input(&bytes, echo_enabled),
            Observation::OutputEof => {
                self.output_eof = true;
                self.post_root_drain_deadline = None;
                Ok(())
            }
            Observation::ProcessSnapshot(result) => self.record_processes(result),
            Observation::Signal(signal) => self.handle_signal(signal),
            Observation::Marker { label, reply } => {
                if self.child_exit.is_some() {
                    let _ = reply.send(Err("run is already stopping".to_owned()));
                    return;
                }
                let result = self.recorder.manual_checkpoint(label);
                match result {
                    Ok(checkpoint) => {
                        let _ = reply.send(Ok(checkpoint));
                        self.dirty.clear_after_checkpoint(&self.recorder.workspace);
                        Ok(())
                    }
                    Err(error) if checkpoint_error_is_nonfatal(&error) => {
                        let _ = reply.send(Err(error.to_string()));
                        Ok(())
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = reply.send(Err(message));
                        Err(error)
                    }
                }
            }
            Observation::ProducerFailed { producer, message } => {
                Err(CaptureError::Producer { producer, message })
            }
        };
        if let Err(error) = result {
            self.fail(error);
        }
    }

    fn record_output(&mut self, bytes: &[u8]) -> Result<(), CaptureError> {
        let byte_len = u64::try_from(bytes.len()).map_err(|_| CaptureError::ClockOutOfRange)?;
        let next = self.terminal_output_bytes.checked_add(byte_len).ok_or(
            CaptureError::TerminalOutputLimit {
                maximum: self.recorder.options.terminal_max_bytes,
            },
        )?;
        if next > self.recorder.options.terminal_max_bytes {
            self.recorder.warning(
                RecorderWarningCode::StorageLimit,
                format!(
                    "Terminal output reached the configured {}-byte limit; capture was stopped.",
                    self.recorder.options.terminal_max_bytes
                ),
            )?;
            self.recorder.flush()?;
            return Err(CaptureError::TerminalOutputLimit {
                maximum: self.recorder.options.terminal_max_bytes,
            });
        }
        let time = timestamp_now()?;
        let stored = self
            .recorder
            .store
            .put_object(bytes, time.as_unix_milliseconds())?;
        self.recorder.emit_object_reference(
            stored.id,
            stored.logical_size,
            EventPayload::TerminalOutput {
                stream_id: self.recorder.stream_id,
                object_id: stored.id,
                byte_len,
            },
        )?;
        self.terminal_output_bytes = next;
        Ok(())
    }

    fn record_input(
        &mut self,
        bytes: &[u8],
        echo_enabled: Option<bool>,
    ) -> Result<(), CaptureError> {
        let byte_len = u64::try_from(bytes.len()).map_err(|_| CaptureError::ClockOutOfRange)?;
        let Some(reason) = input_redaction(self.recorder.options.record_input, echo_enabled) else {
            return self.record_retained_input(bytes, byte_len);
        };
        if reason == InputRedactionReason::EchoDetectionUnavailable && !self.echo_warning_emitted {
            self.recorder.warning(
                RecorderWarningCode::InputEchoDetectionUncertain,
                "Terminal echo state could not be read beside the input read; input bytes were redacted.",
            )?;
            self.echo_warning_emitted = true;
        }
        self.recorder
            .emit(EventPayload::TerminalInputRedacted {
                stream_id: self.recorder.stream_id,
                byte_len,
                reason,
            })
            .map(|_| ())
    }

    fn record_retained_input(&mut self, bytes: &[u8], byte_len: u64) -> Result<(), CaptureError> {
        let time = timestamp_now()?;
        let stored = self
            .recorder
            .store
            .put_object(bytes, time.as_unix_milliseconds())?;
        self.recorder
            .emit_object_reference(
                stored.id,
                stored.logical_size,
                EventPayload::TerminalInput {
                    stream_id: self.recorder.stream_id,
                    object_id: stored.id,
                    byte_len,
                },
            )
            .map(|_| ())
    }

    fn record_processes(
        &mut self,
        result: Result<Vec<ProcessInfo>, String>,
    ) -> Result<(), CaptureError> {
        let processes = match result {
            Ok(processes) => processes,
            Err(message) => {
                if !self.process_warning_emitted {
                    self.recorder.warning(
                        RecorderWarningCode::ProcessObservationIncomplete,
                        sanitize_text(format!("Process observation is incomplete: {message}")),
                    )?;
                    self.process_warning_emitted = true;
                }
                return Ok(());
            }
        };
        let mut present = BTreeSet::new();
        for process in processes {
            let Some(process_id) = ProcessId::new(process.pid) else {
                continue;
            };
            present.insert(process_id);
            if self.observed_processes.insert(process_id) {
                self.recorder.emit(EventPayload::ProcessObserved {
                    process: ProcessObservation {
                        process_id,
                        parent_process_id: process.parent_pid.and_then(ProcessId::new),
                        executable: process
                            .executable
                            .and_then(|path| path.into_os_string().into_string().ok()),
                        command: sanitize_text(process.command),
                        relationship: if process_id == self.root_pid {
                            ProcessRelationship::Root
                        } else {
                            ProcessRelationship::Descendant
                        },
                    },
                })?;
            }
        }
        let exited = self
            .observed_processes
            .iter()
            .copied()
            .filter(|process_id| *process_id != self.root_pid && !present.contains(process_id))
            .collect::<Vec<_>>();
        for process_id in exited {
            self.observed_processes.remove(&process_id);
            self.recorder.emit(EventPayload::ProcessExited {
                process_id,
                status: ProcessExitStatus::Unknown,
            })?;
        }
        Ok(())
    }

    fn handle_signal(&mut self, signal: i32) -> Result<(), CaptureError> {
        if signal == SIGWINCH {
            if self.child_exit.is_none() && self.interactive_stdin {
                match terminal_size(self.stdin_fd)
                    .map_err(CaptureError::from)
                    .and_then(|size| self.master.resize(size).map(|()| size).map_err(Into::into))
                {
                    Ok(size) => {
                        self.recorder.emit(EventPayload::TerminalResized {
                            stream_id: self.recorder.stream_id,
                            columns: size.columns,
                            rows: size.rows,
                        })?;
                    }
                    Err(error) => self.recorder.warning(
                        RecorderWarningCode::Other,
                        sanitize_text(format!("Terminal resize was not propagated: {error}")),
                    )?,
                }
            }
            return Ok(());
        }
        let first_signal = self.interrupted_signal.is_none();
        if first_signal {
            self.interrupted_signal = Some(signal);
            self.recorder.emit(EventPayload::RunInterrupted {
                signal: Some(signal),
            })?;
            self.recorder.flush()?;
        }
        if self.child_exit.is_some() {
            if first_signal {
                self.shorten_post_root_deadline(SIGNAL_GRACE_PERIOD);
                if let Err(error) = self.child.signal_process_group(signal) {
                    self.durable_post_root_warning(format!(
                        "Signal {signal} could not be forwarded to the remaining PTY process group after the root exited: {error}. Rewind will continue bounded shutdown."
                    ));
                }
            } else {
                self.post_root_kill_requested = true;
                self.post_root_drain_deadline = Some(Instant::now() + POST_KILL_DRAIN_GRACE);
                if let Err(error) = self.child.kill_process_group() {
                    self.durable_post_root_warning(format!(
                        "The remaining PTY process group could not be force-terminated after a repeated signal: {error}. Rewind will stop waiting after one final bounded drain."
                    ));
                }
            }
        } else if first_signal {
            self.child.signal_process_group(signal)?;
            self.termination_deadline = Some(Instant::now() + SIGNAL_GRACE_PERIOD);
        } else {
            self.child.kill_process_group()?;
            self.termination_deadline = None;
        }
        Ok(())
    }

    fn poll_child(&mut self) {
        if self.child_exit.is_some() {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(exit)) => {
                self.child_exit = Some(exit);
                self.termination_deadline = None;
                if !self.output_eof {
                    self.post_root_drain_deadline = Some(Instant::now() + POST_ROOT_DRAIN_GRACE);
                }
            }
            Ok(None) => {
                if self
                    .termination_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    if let Err(error) = self.child.kill_process_group() {
                        self.fail(error.into());
                    }
                    self.termination_deadline = None;
                }
            }
            Err(error) => {
                self.fail(error.into());
                self.child_exit = self.child.wait().ok().or_else(|| {
                    Some(ChildExit {
                        code: 1,
                        signal: Some("unobserved termination".to_owned()),
                        signal_number: None,
                    })
                });
                if !self.output_eof {
                    self.post_root_drain_deadline = Some(Instant::now() + POST_ROOT_DRAIN_GRACE);
                }
            }
        }
    }

    fn poll_post_root_drain(&mut self) {
        if self.child_exit.is_none() || self.output_eof || self.output_abandoned {
            return;
        }
        let Some(deadline) = self.post_root_drain_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        if self.post_root_kill_requested {
            self.post_root_drain_deadline = None;
            self.output_abandoned = true;
            self.durable_post_root_warning(
                "The terminal remained open after process-group termination was requested; Rewind stopped waiting and the terminal tail may be incomplete.",
            );
            return;
        }

        self.post_root_kill_requested = true;
        self.post_root_drain_deadline = Some(Instant::now() + POST_KILL_DRAIN_GRACE);
        match self.child.kill_process_group() {
            Ok(()) => self.durable_post_root_warning(
                "The root process exited while the PTY remained open; Rewind requested forced termination of its process group after the bounded drain period.",
            ),
            Err(error) => self.durable_post_root_warning(format!(
                "The root process exited while the PTY remained open; process-group termination failed after the bounded drain period: {error}. Rewind will stop waiting after one final bounded drain."
            )),
        }
    }

    fn terminate_remaining_process_group(&mut self) -> bool {
        match self.child.process_group_exists() {
            Ok(false) => return true,
            Ok(true) => {}
            Err(error) => {
                self.durable_post_root_warning(format!(
                    "The PTY process group could not be checked before the final workspace scan: {error}. No final checkpoint will be committed."
                ));
                self.fail(error.into());
                return false;
            }
        }
        if let Err(error) = self.child.kill_process_group() {
            self.durable_post_root_warning(format!(
                "The PTY process group could not be terminated before the final workspace scan: {error}. No final checkpoint will be committed."
            ));
            self.fail(error.into());
            return false;
        }
        self.durable_post_root_warning(
            "The root process exited while descendants remained in its process group; Rewind terminated them before the final workspace scan.",
        );

        let deadline = Instant::now() + POST_KILL_DRAIN_GRACE;
        loop {
            match self.child.process_group_exists() {
                Ok(false) => return true,
                Ok(true) if Instant::now() < deadline => {}
                Ok(true) => {
                    let error = CaptureError::Producer {
                        producer: "process group shutdown",
                        message: "processes remained after forced termination".to_owned(),
                    };
                    self.durable_post_root_warning(
                        "The PTY process group remained after forced termination; no final checkpoint will be committed.",
                    );
                    self.fail(error);
                    return false;
                }
                Err(error) => {
                    self.durable_post_root_warning(format!(
                        "The PTY process group could not be checked after forced termination: {error}. No final checkpoint will be committed."
                    ));
                    self.fail(error.into());
                    return false;
                }
            }
            match self.receiver.recv_timeout(COORDINATOR_TICK) {
                Ok(observation) => self.handle(observation),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn durable_post_root_warning(&mut self, message: impl Into<String>) {
        let result = self
            .recorder
            .warning(RecorderWarningCode::Other, message)
            .and_then(|()| self.recorder.flush());
        if let Err(error) = result {
            self.fail(error);
        }
    }

    fn shorten_post_root_deadline(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        if self
            .post_root_drain_deadline
            .is_none_or(|current| deadline < current)
        {
            self.post_root_drain_deadline = Some(deadline);
        }
    }

    fn poll_workspace(&mut self) {
        if self.child_exit.is_some() || self.fatal.is_some() || !self.dirty.poll_due() {
            return;
        }
        match self.dirty.observe(&self.recorder.workspace) {
            Ok(paths) => {
                self.dirty_warning_emitted = false;
                for chunk in paths.chunks(MAX_DIRTY_PATHS_PER_EVENT) {
                    match EventPayload::filesystem_paths_dirtied(chunk.to_vec())
                        .map_err(|error| CaptureError::InvalidEvent(error.to_string()))
                        .and_then(|payload| self.recorder.emit(payload).map(|_| ()))
                    {
                        Ok(()) => {}
                        Err(error) => {
                            self.fail(error);
                            return;
                        }
                    }
                }
                if self.dirty.should_checkpoint(&self.recorder.options) {
                    match self
                        .recorder
                        .checkpoint(CheckpointReason::FilesystemQuiescence, None)
                    {
                        Ok(_) => self.dirty.clear_after_checkpoint(&self.recorder.workspace),
                        Err(error) if checkpoint_error_is_nonfatal(&error) => {}
                        Err(error) => self.fail(error),
                    }
                }
            }
            Err(error) => {
                if !self.dirty_warning_emitted {
                    let action = if self.dirty.disabled() {
                        "Dirty-path hint scanning is disabled for this run; final checkpoint scanning remains authoritative"
                    } else {
                        "Dirty-path scan will retry"
                    };
                    if let Err(warning_error) = self.recorder.warning(
                        RecorderWarningCode::FilesystemRace,
                        sanitize_text(format!("{action}: {error}")),
                    ) {
                        self.fail(warning_error);
                    }
                    self.dirty_warning_emitted = true;
                }
            }
        }
    }

    fn fail(&mut self, error: CaptureError) {
        if self.fatal.is_none() {
            self.fatal = Some(error);
            if self.child_exit.is_none() && self.child.kill_process_group().is_err() {
                let _ = self.child.kill();
            }
        }
    }
}

fn input_redaction(
    policy: InputRecordingPolicy,
    echo_enabled: Option<bool>,
) -> Option<InputRedactionReason> {
    match (policy, echo_enabled) {
        (InputRecordingPolicy::Always, _) | (InputRecordingPolicy::Auto, Some(true)) => None,
        (InputRecordingPolicy::Never, _) => Some(InputRedactionReason::PolicyNever),
        (InputRecordingPolicy::Auto, Some(false)) => Some(InputRedactionReason::EchoDisabled),
        (InputRecordingPolicy::Auto, None) => Some(InputRedactionReason::EchoDetectionUnavailable),
    }
}

fn drain_ready(coordinator: &mut Coordinator<'_, '_>) {
    while let Ok(observation) = coordinator.receiver.try_recv() {
        coordinator.handle(observation);
    }
}

fn join_output_draining(
    output: JoinHandle<()>,
    coordinator: &mut Coordinator<'_, '_>,
) -> Result<(), CaptureError> {
    while !output.is_finished() {
        match coordinator.receiver.recv_timeout(COORDINATOR_TICK) {
            Ok(observation) => coordinator.handle(observation),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }
    drain_ready(coordinator);
    output
        .join()
        .map_err(|_| CaptureError::WorkerPanicked("terminal output"))
}

struct Recorder<'store> {
    store: &'store mut Store,
    run_id: RunId,
    stream_id: TerminalStreamId,
    workspace: PathBuf,
    options: CaptureOptions,
    sequencer: Sequencer,
    pending: Vec<Event>,
    checkpoint_count: u64,
    excluded_paths: BTreeSet<SnapshotPath>,
    accounted_objects: BTreeSet<ObjectId>,
    logical_object_bytes: u64,
}

impl<'store> Recorder<'store> {
    fn new(
        store: &'store mut Store,
        run_id: RunId,
        stream_id: TerminalStreamId,
        workspace: PathBuf,
        options: CaptureOptions,
        started: Instant,
    ) -> Self {
        Self {
            store,
            run_id,
            stream_id,
            workspace,
            options,
            sequencer: Sequencer::new(run_id, started),
            pending: Vec::new(),
            checkpoint_count: 0,
            excluded_paths: BTreeSet::new(),
            accounted_objects: BTreeSet::new(),
            logical_object_bytes: 0,
        }
    }

    fn emit(&mut self, payload: EventPayload) -> Result<EventSequence, CaptureError> {
        let sequence = self.queue_event(payload)?;
        if self.pending.len() >= self.options.event_batch_size {
            self.flush()?;
        }
        Ok(sequence)
    }

    fn queue_event(&mut self, payload: EventPayload) -> Result<EventSequence, CaptureError> {
        let event = self.sequencer.next(payload)?;
        let sequence = event.sequence;
        self.pending.push(event);
        Ok(sequence)
    }

    fn warning(
        &mut self,
        code: RecorderWarningCode,
        message: impl Into<String>,
    ) -> Result<(), CaptureError> {
        self.emit(EventPayload::RecorderWarning {
            warning: RecorderWarning {
                code,
                message: sanitize_text(message.into()),
            },
        })
        .map(|_| ())
    }

    fn emit_object_reference(
        &mut self,
        object_id: ObjectId,
        logical_size: u64,
        payload: EventPayload,
    ) -> Result<EventSequence, CaptureError> {
        let candidates = BTreeMap::from([(object_id, logical_size)]);
        let projected = match self.projected_object_bytes(&candidates) {
            Ok(projected) => projected,
            Err(error) => {
                self.warn_run_storage_limit()?;
                self.flush()?;
                return Err(error);
            }
        };
        let sequence = self.emit(payload)?;
        self.account_objects(&candidates, projected);
        Ok(sequence)
    }

    fn projected_object_bytes(
        &self,
        candidates: &BTreeMap<ObjectId, u64>,
    ) -> Result<u64, CaptureError> {
        let additional = candidates
            .iter()
            .filter(|(object_id, _)| !self.accounted_objects.contains(object_id))
            .try_fold(0_u64, |total, (_, logical_size)| {
                total.checked_add(*logical_size)
            })
            .ok_or(CaptureError::RunStorageLimit {
                maximum: self.options.max_run_bytes,
            })?;
        let projected = self.logical_object_bytes.checked_add(additional).ok_or(
            CaptureError::RunStorageLimit {
                maximum: self.options.max_run_bytes,
            },
        )?;
        if projected > self.options.max_run_bytes {
            return Err(CaptureError::RunStorageLimit {
                maximum: self.options.max_run_bytes,
            });
        }
        Ok(projected)
    }

    fn account_objects(&mut self, candidates: &BTreeMap<ObjectId, u64>, projected: u64) {
        self.accounted_objects.extend(candidates.keys().copied());
        self.logical_object_bytes = projected;
    }

    fn warn_run_storage_limit(&mut self) -> Result<(), CaptureError> {
        self.warning(
            RecorderWarningCode::StorageLimit,
            format!(
                "Run objects reached the configured {}-byte unique logical storage limit; capture was stopped.",
                self.options.max_run_bytes
            ),
        )
    }

    fn warn_exclusions(&mut self, count: usize) -> Result<(), CaptureError> {
        if count == 0 {
            return Ok(());
        }
        self.warning(
            RecorderWarningCode::Other,
            format!(
                "Snapshot policy excluded {count} workspace path(s); replay will show incomplete content and the retained workspace entry will be removed after finalization."
            ),
        )
    }

    fn flush(&mut self) -> Result<(), CaptureError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.store.append_event_batch(&self.pending)?;
        self.pending.clear();
        Ok(())
    }

    fn checkpoint(
        &mut self,
        reason: CheckpointReason,
        label: Option<String>,
    ) -> Result<CheckpointCommit, CaptureError> {
        let checkpoint_id = CheckpointId::generate();
        self.checkpoint_allocated(checkpoint_id, reason, label)
    }

    fn checkpoint_allocated(
        &mut self,
        checkpoint_id: CheckpointId,
        reason: CheckpointReason,
        label: Option<String>,
    ) -> Result<CheckpointCommit, CaptureError> {
        self.emit(EventPayload::CheckpointStarted {
            checkpoint_id,
            reason,
        })?;
        self.flush()?;

        // ponytail: scans run synchronously; bounded PTY backpressure preserves
        // correctness. Move CAS-only scan work to a worker if profiling shows
        // interactive stalls on large workspaces.
        let created = timestamp_now()?;
        let report = match scan_workspace(
            &self.workspace,
            self.store,
            &self.options.snapshot,
            created.as_unix_milliseconds(),
        ) {
            Ok(report) => report,
            Err(error) => {
                self.emit(EventPayload::CheckpointFailed {
                    checkpoint_id,
                    failure: RecorderFailure {
                        kind: RecorderFailureKind::Snapshot,
                        message:
                            "Authoritative workspace scan failed; no checkpoint was committed."
                                .to_owned(),
                    },
                })?;
                self.flush()?;
                return Err(error.into());
            }
        };
        self.commit_scanned(checkpoint_id, reason, label, report)
    }

    fn manual_checkpoint(&mut self, label: Option<String>) -> Result<CheckpointId, CaptureError> {
        if let Some(label) = &label {
            let checkpoint_id = CheckpointId::generate();
            self.emit(EventPayload::MarkerCreated {
                checkpoint_id,
                label: label.clone(),
            })?;
            return self
                .checkpoint_allocated(checkpoint_id, CheckpointReason::Manual, Some(label.clone()))
                .map(|commit| commit.id);
        }
        self.checkpoint(CheckpointReason::Manual, None)
            .map(|commit| commit.id)
    }

    fn commit_scanned(
        &mut self,
        checkpoint_id: CheckpointId,
        reason: CheckpointReason,
        label: Option<String>,
        report: ScanReport,
    ) -> Result<CheckpointCommit, CaptureError> {
        let exclusion_count = report.exclusions.len();
        self.excluded_paths.extend(
            report
                .exclusions
                .iter()
                .map(|excluded| excluded.path.clone()),
        );
        let objects = report
            .snapshot
            .manifest
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                SnapshotEntryKind::File {
                    object_id, size, ..
                } => Some((*object_id, *size)),
                SnapshotEntryKind::Directory | SnapshotEntryKind::Symlink { .. } => None,
            })
            .collect::<BTreeMap<_, _>>();
        let projected = match self.projected_object_bytes(&objects) {
            Ok(projected) => projected,
            Err(error) => {
                self.warn_run_storage_limit()?;
                self.emit(EventPayload::CheckpointFailed {
                    checkpoint_id,
                    failure: RecorderFailure {
                        kind: RecorderFailureKind::ResourceLimit,
                        message: "Run storage limit prevented this checkpoint from committing."
                            .to_owned(),
                    },
                })?;
                self.flush()?;
                return Err(error);
            }
        };
        let committed = self.sequencer.next(EventPayload::CheckpointCommitted {
            checkpoint_id,
            snapshot_id: report.snapshot.id,
        })?;
        let checkpoint = Checkpoint {
            id: checkpoint_id,
            run_id: self.run_id,
            sequence: committed.sequence,
            label,
            reason,
            snapshot_id: report.snapshot.id,
            created_at: committed.wall_clock,
            monotonic_offset: committed.monotonic_offset,
        };
        self.store
            .commit_checkpoint_with_event(&checkpoint, &report.snapshot, &committed)?;
        self.account_objects(&objects, projected);
        self.checkpoint_count = self
            .checkpoint_count
            .checked_add(1)
            .ok_or(CaptureError::ClockOutOfRange)?;
        Ok(CheckpointCommit {
            id: checkpoint_id,
            snapshot_id: report.snapshot.id,
            exclusions: exclusion_count,
        })
    }

    fn finish(
        &mut self,
        status: RunStatus,
        exit_status: ProcessExitStatus,
        final_snapshot: Option<SnapshotId>,
    ) -> Result<(), CaptureError> {
        self.queue_event(EventPayload::RunCompleted {
            status,
            exit_status: Some(exit_status),
        })?;
        let (finished_at, duration) = self.sequencer.now()?;
        self.commit_finish(RunFinish {
            status,
            finished_at,
            monotonic_duration: duration,
            final_snapshot,
            exit_status: Some(exit_status),
        })
    }

    fn finish_preparation_failure(&mut self, message: &str) {
        let _ = self.warning(
            RecorderWarningCode::Other,
            sanitize_text(message.to_owned()),
        );
        if let Err(error) = remove_path_no_follow(&self.workspace) {
            self.record_cleanup_failure(1, &error);
        }
        let _ = self.queue_event(EventPayload::RunCompleted {
            status: RunStatus::Failed,
            exit_status: None,
        });
        if let Ok((finished_at, monotonic_duration)) = self.sequencer.now() {
            let _ = self.commit_finish(RunFinish {
                status: RunStatus::Failed,
                finished_at,
                monotonic_duration,
                final_snapshot: None,
                exit_status: None,
            });
        }
    }

    fn finish_running_without_child(&mut self, initial_snapshot: SnapshotId, message: &str) {
        let _ = self.warning(
            RecorderWarningCode::Other,
            sanitize_text(message.to_owned()),
        );
        let final_snapshot = match self.checkpoint(CheckpointReason::Final, None) {
            Ok(commit) => {
                let _ = self.warn_exclusions(commit.exclusions);
                Some(commit.snapshot_id)
            }
            Err(_) => Some(initial_snapshot),
        };
        let _ = self.cleanup_exclusions_for_retention();
        let _ = self.queue_event(EventPayload::RunCompleted {
            status: RunStatus::Failed,
            exit_status: None,
        });
        if let Ok((finished_at, monotonic_duration)) = self.sequencer.now() {
            let _ = self.commit_finish(RunFinish {
                status: RunStatus::Failed,
                finished_at,
                monotonic_duration,
                final_snapshot,
                exit_status: None,
            });
        }
    }

    fn commit_finish(&mut self, finish: RunFinish) -> Result<(), CaptureError> {
        self.store
            .finish_run_with_events(self.run_id, finish, &self.pending)?;
        self.pending.clear();
        Ok(())
    }

    fn cleanup_exclusions_for_retention(&mut self) -> Result<(), CaptureError> {
        let paths = self
            .excluded_paths
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(());
        }
        let root = match DirectoryRoot::open(&self.workspace) {
            Ok(root) => root,
            Err(error) => {
                let failed = paths.len();
                let source = io::Error::other(error);
                self.record_cleanup_failure(failed, &source);
                return Err(CaptureError::PrivacyCleanup { failed, source });
            }
        };
        let mut failed = 0;
        let mut first_error = None;
        for path in paths {
            match remove_relative_entry(&root, path.as_str()) {
                Ok(()) => {
                    self.excluded_paths.remove(&path);
                }
                Err(error) => {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(io::Error::other(error));
                    }
                }
            }
        }
        let Some(source) = first_error else {
            return Ok(());
        };
        self.record_cleanup_failure(failed, &source);
        Err(CaptureError::PrivacyCleanup { failed, source })
    }

    fn record_cleanup_failure(&mut self, failed: usize, source: &io::Error) {
        let _ = self.warning(
            RecorderWarningCode::PrivacyCleanupFailed,
            format!(
                "Could not remove {failed} excluded path(s) from the retained run workspace: {source}. Inspect or remove that workspace before sharing it."
            ),
        );
    }
}

fn remove_path_no_follow(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

struct CheckpointCommit {
    id: CheckpointId,
    snapshot_id: SnapshotId,
    exclusions: usize,
}

struct Sequencer {
    run_id: RunId,
    next: EventSequence,
    started: Instant,
    last_offset: MonotonicDuration,
}

impl Sequencer {
    fn new(run_id: RunId, started: Instant) -> Self {
        Self {
            run_id,
            next: EventSequence::FIRST,
            started,
            last_offset: MonotonicDuration::ZERO,
        }
    }

    fn next(&mut self, payload: EventPayload) -> Result<Event, CaptureError> {
        let (wall_clock, monotonic_offset) = self.now()?;
        self.next_at(wall_clock, monotonic_offset, payload)
    }

    fn next_at(
        &mut self,
        wall_clock: Timestamp,
        monotonic_offset: MonotonicDuration,
        payload: EventPayload,
    ) -> Result<Event, CaptureError> {
        let monotonic_offset = monotonic_offset.max(self.last_offset);
        let event = Event::new(
            self.run_id,
            self.next,
            wall_clock,
            monotonic_offset,
            payload,
        )
        .map_err(|error| CaptureError::InvalidEvent(error.to_string()))?;
        self.next = self
            .next
            .checked_next()
            .ok_or(CaptureError::EventSequenceExhausted)?;
        self.last_offset = monotonic_offset;
        Ok(event)
    }

    fn now(&self) -> Result<(Timestamp, MonotonicDuration), CaptureError> {
        let nanoseconds = u64::try_from(self.started.elapsed().as_nanos())
            .map_err(|_| CaptureError::ClockOutOfRange)?;
        Ok((
            timestamp_now()?,
            MonotonicDuration::from_nanoseconds(nanoseconds),
        ))
    }
}

struct ProcessProducer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessProducer {
    fn start(
        root_pid: u32,
        sender: SyncSender<Observation>,
        interval: Duration,
    ) -> Result<Self, CaptureError> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("rewind-processes".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    let result = supervised_processes(root_pid).map_err(|error| error.to_string());
                    if !send_until_stopped(
                        &sender,
                        Observation::ProcessSnapshot(result),
                        &thread_stop,
                    ) {
                        break;
                    }
                    sleep_interruptibly(&thread_stop, interval);
                }
            })
            .map_err(|error| CaptureError::Producer {
                producer: "process observer",
                message: error.to_string(),
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| CaptureError::WorkerPanicked("process observer"))?;
        }
        Ok(())
    }
}

impl Drop for ProcessProducer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct SignalProducer {
    handle: SignalHandle,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SignalProducer {
    fn start(sender: SyncSender<Observation>) -> Result<Self, CaptureError> {
        let mut signals =
            Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGWINCH]).map_err(|error| {
                CaptureError::Producer {
                    producer: "signal handler",
                    message: error.to_string(),
                }
            })?;
        let handle = signals.handle();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("rewind-signals".to_owned())
            .spawn(move || {
                for signal in signals.forever() {
                    if !send_until_stopped(&sender, Observation::Signal(signal), &thread_stop) {
                        break;
                    }
                }
            })
            .map_err(|error| CaptureError::Producer {
                producer: "signal handler",
                message: error.to_string(),
            })?;
        Ok(Self {
            handle,
            stop,
            thread: Some(thread),
        })
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.handle.close();
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| CaptureError::WorkerPanicked("signal handler"))?;
        }
        Ok(())
    }
}

impl Drop for SignalProducer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn spawn_output(
    mut reader: Box<dyn Read + Send>,
    sender: SyncSender<Observation>,
    chunk_size: usize,
) -> Result<JoinHandle<()>, CaptureError> {
    thread::Builder::new()
        .name("rewind-terminal-output".to_owned())
        .spawn(move || {
            let stdout = io::stdout();
            let mut buffer = vec![0_u8; chunk_size];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = sender.send(Observation::OutputEof);
                        break;
                    }
                    Ok(read) => {
                        let bytes = buffer[..read].to_vec();
                        if sender
                            .send(Observation::TerminalOutput(bytes.clone()))
                            .is_err()
                        {
                            break;
                        }
                        let mut stdout = stdout.lock();
                        if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
                            let _ = sender.send(Observation::ProducerFailed {
                                producer: "terminal output forwarding",
                                message: error.to_string(),
                            });
                            let _ = sender.send(Observation::OutputEof);
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let _ = sender.send(Observation::ProducerFailed {
                            producer: "PTY output",
                            message: error.to_string(),
                        });
                        let _ = sender.send(Observation::OutputEof);
                        break;
                    }
                }
            }
        })
        .map_err(|error| CaptureError::Producer {
            producer: "terminal output",
            message: error.to_string(),
        })
}

fn spawn_input(
    mut writer: Box<dyn Write + Send>,
    echo_probe: Option<PtyEchoProbe>,
    sender: SyncSender<Observation>,
    chunk_size: usize,
) -> Result<(), CaptureError> {
    thread::Builder::new()
        .name("rewind-terminal-input".to_owned())
        .spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            let mut buffer = vec![0_u8; chunk_size.min(8 * 1024)];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        // Sample before forwarding: the child can re-enable echo
                        // immediately after consuming a secret prompt response.
                        let echo_enabled = echo_probe.as_ref().and_then(PtyEchoProbe::echo_enabled);
                        if let Err(error) = writer
                            .write_all(&buffer[..read])
                            .and_then(|()| writer.flush())
                        {
                            let _ = sender.send(Observation::ProducerFailed {
                                producer: "terminal input forwarding",
                                message: error.to_string(),
                            });
                            break;
                        }
                        if sender
                            .send(Observation::TerminalInput {
                                bytes: buffer[..read].to_vec(),
                                echo_enabled,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let _ = sender.send(Observation::ProducerFailed {
                            producer: "terminal input",
                            message: error.to_string(),
                        });
                        break;
                    }
                }
            }
        })
        .map_err(|error| CaptureError::Producer {
            producer: "terminal input",
            message: error.to_string(),
        })?;
    // ponytail: blocking standard input has no portable cancellation hook.
    // CLI captures are process-scoped; add a platform pollable reader if the
    // library is later used for many interactive runs in one process.
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    kind: u8,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    symlink_target: Option<Vec<u8>>,
}

struct DirtyTracker {
    observed: BTreeMap<SnapshotPath, FileStamp>,
    pending: BTreeSet<SnapshotPath>,
    last_change: Option<Instant>,
    last_checkpoint: Instant,
    next_poll: Instant,
    poll_interval: Duration,
    maximum_pending: usize,
    deferred_error: Option<CaptureError>,
    disabled: bool,
}

impl DirtyTracker {
    fn new(workspace: &Path, options: &CaptureOptions) -> Result<Self, CaptureError> {
        let now = Instant::now();
        let poll_interval = (options.checkpoint_debounce / 2)
            .max(Duration::from_millis(25))
            .min(Duration::from_millis(250));
        let (observed, deferred_error) = match workspace_state(workspace) {
            Ok(state) => (state, None),
            Err(error) if is_dirty_scan_limit(&error) => (BTreeMap::new(), Some(error)),
            Err(error) => return Err(error),
        };
        Ok(Self {
            observed,
            pending: BTreeSet::new(),
            last_change: None,
            last_checkpoint: now,
            next_poll: now + poll_interval,
            poll_interval,
            maximum_pending: options.maximum_pending_dirty_paths,
            deferred_error,
            disabled: false,
        })
    }

    fn poll_due(&self) -> bool {
        !self.disabled && Instant::now() >= self.next_poll
    }

    fn observe(&mut self, workspace: &Path) -> Result<Vec<SnapshotPath>, CaptureError> {
        if let Some(error) = self.deferred_error.take() {
            self.disabled = true;
            return Err(error);
        }
        let now = Instant::now();
        self.next_poll = now + self.poll_interval;
        let next = match workspace_state(workspace) {
            Ok(next) => next,
            Err(error) if is_dirty_scan_limit(&error) => {
                self.disabled = true;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let mut changed = Vec::new();
        let mut any_changed = false;
        for (path, previous) in &self.observed {
            if next.get(path) != Some(previous) {
                any_changed = true;
                if self.pending.len() < self.maximum_pending && self.pending.insert(path.clone()) {
                    changed.push(path.clone());
                }
            }
        }
        for path in next.keys() {
            if !self.observed.contains_key(path) {
                any_changed = true;
                if self.pending.len() < self.maximum_pending && self.pending.insert(path.clone()) {
                    changed.push(path.clone());
                }
            }
        }
        if any_changed {
            self.last_change = Some(now);
        }
        self.observed = next;
        Ok(changed)
    }

    fn should_checkpoint(&self, options: &CaptureOptions) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let now = Instant::now();
        self.pending.len() >= options.maximum_pending_dirty_paths
            || now.duration_since(self.last_checkpoint) >= options.checkpoint_max_interval
            || (self
                .last_change
                .is_some_and(|last| now.duration_since(last) >= options.checkpoint_debounce)
                && now.duration_since(self.last_checkpoint) >= options.checkpoint_min_interval)
    }

    fn clear_after_checkpoint(&mut self, workspace: &Path) {
        if !self.disabled {
            match workspace_state(workspace) {
                Ok(state) => self.observed = state,
                Err(error) if is_dirty_scan_limit(&error) => {
                    self.deferred_error = Some(error);
                    self.next_poll = Instant::now();
                }
                Err(_) => {}
            }
        }
        self.pending.clear();
        self.last_change = None;
        self.last_checkpoint = Instant::now();
        self.next_poll = Instant::now() + self.poll_interval;
    }

    fn disabled(&self) -> bool {
        self.disabled
    }
}

fn workspace_state(root: &Path) -> Result<BTreeMap<SnapshotPath, FileStamp>, CaptureError> {
    workspace_state_bounded(root, MAX_DIRTY_SCAN_ENTRIES)
}

fn workspace_state_bounded(
    root: &Path,
    maximum_entries: usize,
) -> Result<BTreeMap<SnapshotPath, FileStamp>, CaptureError> {
    let directory_root = DirectoryRoot::open(root)?;
    let directory = directory_root.pinned_directory()?;
    let mut state = BTreeMap::new();
    walk_state(root, &directory, "", &mut state, maximum_entries)?;
    Ok(state)
}

fn walk_state(
    root: &Path,
    directory: &PinnedDirectory,
    relative: &str,
    state: &mut BTreeMap<SnapshotPath, FileStamp>,
    maximum_entries: usize,
) -> Result<(), CaptureError> {
    let remaining = maximum_entries.saturating_sub(state.len());
    for entry in directory.entries_bounded(remaining)? {
        if state.len() == maximum_entries {
            return Err(dirty_scan_limit(maximum_entries));
        }
        let name = entry
            .name()
            .to_str()
            .ok_or_else(|| CaptureError::Io {
                operation: "represent workspace path beneath",
                path: root.join(relative).join(entry.name()),
                source: io::Error::new(io::ErrorKind::InvalidData, "path is not valid UTF-8"),
            })?
            .to_owned();
        let relative = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        let snapshot_path =
            relative
                .parse::<SnapshotPath>()
                .map_err(|error| CaptureError::Producer {
                    producer: "dirty-path scanner",
                    message: error.to_string(),
                })?;
        let metadata = entry.metadata();
        let symlink_target = if metadata.kind == DirectoryEntryKind::Symlink {
            Some(
                directory
                    .read_symlink(&entry)?
                    .as_os_str()
                    .as_bytes()
                    .to_vec(),
            )
        } else {
            None
        };
        state.insert(
            snapshot_path,
            FileStamp {
                kind: match metadata.kind {
                    DirectoryEntryKind::File => 1,
                    DirectoryEntryKind::Directory => 2,
                    DirectoryEntryKind::Symlink => 3,
                    DirectoryEntryKind::Other => 4,
                },
                mode: metadata.mode,
                size: metadata.size,
                modified_seconds: metadata.modified_seconds,
                modified_nanoseconds: metadata.modified_nanoseconds,
                changed_seconds: metadata.changed_seconds,
                changed_nanoseconds: metadata.changed_nanoseconds,
                symlink_target,
            },
        );
        if metadata.kind == DirectoryEntryKind::Directory {
            let child = directory.open_directory(&entry)?;
            walk_state(root, &child, &relative, state, maximum_entries)?;
            directory.verify_entry(&entry)?;
        } else if metadata.kind != DirectoryEntryKind::Symlink {
            directory.verify_entry(&entry)?;
        }
    }
    directory.verify_unchanged()?;
    Ok(())
}

fn dirty_scan_limit(maximum_entries: usize) -> CaptureError {
    CaptureError::Producer {
        producer: "dirty-path scanner",
        message: format!("workspace exceeds the {maximum_entries}-entry hint scan budget"),
    }
}

fn is_dirty_scan_limit(error: &CaptureError) -> bool {
    matches!(
        error,
        CaptureError::Workspace(rewind_platform::FileSystemError::DirectoryEntryLimit { .. })
    ) || matches!(
        error,
        CaptureError::Producer {
            producer: "dirty-path scanner",
            message,
        } if message.contains("entry hint scan budget")
    )
}

fn sleep_interruptibly(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

fn set_environment(
    environment: &mut Vec<(OsString, OsString)>,
    key: impl AsRef<OsStr>,
    value: impl Into<OsString>,
) {
    let key = key.as_ref();
    environment.retain(|(candidate, _)| candidate != key);
    environment.push((key.to_os_string(), value.into()));
}

fn canonicalize(operation: &'static str, path: &Path) -> Result<PathBuf, CaptureError> {
    fs::canonicalize(path).map_err(|source| CaptureError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })
}

fn current_platform() -> Result<Platform, CaptureError> {
    let capabilities = rewind_platform::capabilities();
    match (capabilities.operating_system, capabilities.architecture) {
        ("macos", "aarch64") => Ok(Platform::MacOsAarch64),
        ("linux", "x86_64") => Ok(Platform::LinuxX86_64),
        ("linux", "aarch64") => Ok(Platform::LinuxAarch64),
        _ => Err(CaptureError::Producer {
            producer: "platform detection",
            message: format!(
                "unsupported host {} {}",
                capabilities.operating_system, capabilities.architecture
            ),
        }),
    }
}

fn timestamp_now() -> Result<Timestamp, CaptureError> {
    let milliseconds = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            i128::try_from(duration.as_millis()).map_err(|_| CaptureError::ClockOutOfRange)?
        }
        Err(error) => -i128::try_from(error.duration().as_millis())
            .map_err(|_| CaptureError::ClockOutOfRange)?,
    };
    let milliseconds = i64::try_from(milliseconds).map_err(|_| CaptureError::ClockOutOfRange)?;
    Ok(Timestamp::from_unix_milliseconds(milliseconds))
}

fn domain_exit(exit: &ChildExit) -> ProcessExitStatus {
    if let Some(signal) = exit.signal_number {
        ProcessExitStatus::Signal(signal)
    } else if exit.signal.is_some() {
        ProcessExitStatus::Unknown
    } else {
        i32::try_from(exit.code)
            .map(ProcessExitStatus::Code)
            .unwrap_or(ProcessExitStatus::Unknown)
    }
}

fn sanitize_text(mut message: String) -> String {
    if message.is_empty() {
        return "Recorder operation failed without a diagnostic.".to_owned();
    }
    if message.len() <= MAX_EVENT_TEXT_BYTES {
        return message;
    }
    let mut end = MAX_EVENT_TEXT_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

fn checkpoint_error_is_nonfatal(error: &CaptureError) -> bool {
    matches!(error, CaptureError::Snapshot(snapshot) if !matches!(snapshot.as_ref(), SnapshotError::Store(_)))
}

fn is_store_failure(error: &CaptureError) -> bool {
    matches!(error, CaptureError::Store(_))
        || matches!(error, CaptureError::Snapshot(snapshot) if matches!(snapshot.as_ref(), SnapshotError::Store(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::mpsc;

    fn warning_payload() -> EventPayload {
        EventPayload::RecorderWarning {
            warning: RecorderWarning {
                code: RecorderWarningCode::Other,
                message: "test".to_owned(),
            },
        }
    }

    #[test]
    fn sequencer_is_contiguous_when_wall_clock_moves_backward() {
        let run_id = RunId::generate();
        let mut sequencer = Sequencer::new(run_id, Instant::now());
        let first = sequencer
            .next_at(
                Timestamp::from_unix_milliseconds(100),
                MonotonicDuration::from_nanoseconds(20),
                warning_payload(),
            )
            .unwrap();
        let second = sequencer
            .next_at(
                Timestamp::from_unix_milliseconds(10),
                MonotonicDuration::from_nanoseconds(10),
                warning_payload(),
            )
            .unwrap();
        assert_eq!(first.sequence.get(), 1);
        assert_eq!(second.sequence.get(), 2);
        assert_eq!(second.wall_clock.as_unix_milliseconds(), 10);
        assert_eq!(second.monotonic_offset.as_nanoseconds(), 20);
    }

    #[test]
    fn bounded_channel_blocks_instead_of_dropping() {
        let (sender, receiver) = sync_channel(1);
        sender.send(1_u8).unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let child_completed = Arc::clone(&completed);
        let thread = thread::spawn(move || {
            sender.send(2).unwrap();
            child_completed.store(true, Ordering::Release);
        });
        thread::sleep(Duration::from_millis(20));
        assert!(!completed.load(Ordering::Acquire));
        assert_eq!(receiver.recv().unwrap(), 1);
        thread.join().unwrap();
        assert_eq!(receiver.recv().unwrap(), 2);
    }

    #[test]
    fn stopped_producer_leaves_a_full_bounded_channel() {
        let (sender, receiver) = sync_channel(1);
        sender.send(Observation::OutputEof).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(Barrier::new(2));
        let thread_stop = Arc::clone(&stop);
        let thread_barrier = Arc::clone(&barrier);
        let (done_sender, done_receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            thread_barrier.wait();
            let sent = send_until_stopped(&sender, Observation::OutputEof, &thread_stop);
            let _ = done_sender.send(sent);
        });
        barrier.wait();
        stop.store(true, Ordering::Release);

        match done_receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(sent) => assert!(!sent),
            Err(error) => {
                drop(receiver);
                thread.join().unwrap();
                panic!("stopped producer remained blocked: {error}");
            }
        }
        thread.join().unwrap();
        assert!(matches!(receiver.recv().unwrap(), Observation::OutputEof));
    }

    #[test]
    fn terminal_restore_failure_preserves_an_existing_capture_failure() {
        let restore = rewind_platform::TerminalError::Io {
            operation: "restore mode",
            source: io::Error::other("fixture restore failure"),
        };
        let error = combine_terminal_restore::<()>(Err(CaptureError::InvalidCommand), Err(restore))
            .unwrap_err();
        assert!(matches!(
            error,
            CaptureError::TerminalRestoreAfterFailure { primary, .. }
                if matches!(*primary, CaptureError::InvalidCommand)
        ));
    }

    #[test]
    fn input_retention_uses_the_echo_sample_taken_beside_the_read() {
        assert_eq!(
            input_redaction(InputRecordingPolicy::Auto, Some(false)),
            Some(InputRedactionReason::EchoDisabled)
        );
        assert_eq!(
            input_redaction(InputRecordingPolicy::Auto, None),
            Some(InputRedactionReason::EchoDetectionUnavailable)
        );
        assert_eq!(
            input_redaction(InputRecordingPolicy::Auto, Some(true)),
            None
        );
        assert_eq!(
            input_redaction(InputRecordingPolicy::Never, Some(true)),
            Some(InputRedactionReason::PolicyNever)
        );
        assert_eq!(
            input_redaction(InputRecordingPolicy::Always, Some(false)),
            None
        );
    }

    #[test]
    fn excluded_path_cleanup_never_follows_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
        let workspace_root = DirectoryRoot::open(&workspace).unwrap();

        remove_relative_entry(&workspace_root, "link/secret").unwrap();
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside");

        remove_relative_entry(&workspace_root, "link").unwrap();
        assert!(!workspace.join("link").exists());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside");
    }

    #[test]
    fn dirty_tracker_reports_content_metadata_changes() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"before").unwrap();
        let before = workspace_state(temp.path()).unwrap();
        fs::write(temp.path().join("file"), b"after-longer").unwrap();
        let after = workspace_state(temp.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn dirty_tracker_is_descriptor_bounded_and_does_not_follow_links() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();
        fs::write(workspace.join("one"), b"one").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();

        let state = workspace_state_bounded(&workspace, 2).unwrap();
        assert!(state.contains_key(&"one".parse().unwrap()));
        assert!(state.contains_key(&"link".parse().unwrap()));
        assert!(!state.contains_key(&"link/sentinel".parse().unwrap()));

        let error = workspace_state_bounded(&workspace, 1).unwrap_err();
        assert!(is_dirty_scan_limit(&error));
    }
}
