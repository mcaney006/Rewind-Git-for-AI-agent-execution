use std::ffi::OsString;
use std::fs;
use std::io;
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rewind_capture::{CaptureError, CaptureRequest, capture, control_socket_path, request_marker};
use rewind_domain::{
    CheckpointReason, EventPayload, ObjectId, ProcessExitStatus, ProcessRelationship,
    RecorderFailureKind, RunStatus, WorkspaceCloneStrategy,
};
use rewind_platform::CloneStrategy;
use rewind_store::Store;

#[test]
fn real_pty_capture_isolates_marks_checkpoints_and_persists_output() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store_root = temp.path().join("store");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"source\n").unwrap();
    let ignored_secret = b"private fixture value";
    let initially_large = b"initial content above the fixture limit";
    fs::write(source.join("secret.txt"), ignored_secret).unwrap();
    fs::write(source.join("once-large.bin"), initially_large).unwrap();
    let release = temp.path().join("release-child");

    let mut request = CaptureRequest::new(&source, "/bin/sh");
    request.arguments = vec![
        OsString::from("-c"),
        OsString::from(
            "printf 'hello'; i=0; while [ ! -e \"$REWIND_TEST_RELEASE\" ] && [ \"$i\" -lt 300 ]; do sleep 0.01; i=$((i + 1)); done; [ -e \"$REWIND_TEST_RELEASE\" ] || exit 88; printf 'isolated\\n' > file; printf 'small' > once-large.bin; sleep 0.4; exit 7",
        ),
    ];
    request.extra_environment.push((
        OsString::from("REWIND_TEST_RELEASE"),
        release.clone().into_os_string(),
    ));
    request
        .options
        .snapshot
        .ignore
        .push("secret.txt".parse().unwrap());
    request.options.snapshot.max_file_size = 16;
    request.options.checkpoint_debounce = Duration::from_millis(50);
    request.options.checkpoint_min_interval = Duration::from_millis(50);
    request.options.checkpoint_max_interval = Duration::from_millis(200);
    request.options.process_poll_interval = Duration::from_millis(20);
    let capture_store = store_root.clone();
    let capture_thread = thread::spawn(move || capture(&capture_store, request));

    let socket = control_socket_path(&store_root);
    let deadline = Instant::now() + Duration::from_secs(3);
    let marker = loop {
        match request_marker(&socket, Some("before mutation".to_owned())) {
            Ok(marker) => break marker,
            Err(rewind_capture::ControlClientError::Io {
                operation: "connect",
                source,
                ..
            }) if matches!(
                source.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("control socket did not become ready: {error}"),
        }
    };
    fs::write(&release, b"release").unwrap();
    let outcome = capture_thread.join().unwrap().unwrap();

    assert_eq!(marker.run_id, outcome.run_id);
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(outcome.exit_status, ProcessExitStatus::Code(7));
    assert_eq!(fs::read(source.join("file")).unwrap(), b"source\n");
    assert_eq!(fs::read(source.join("secret.txt")).unwrap(), ignored_secret);
    assert_eq!(
        fs::read(source.join("once-large.bin")).unwrap(),
        initially_large
    );
    assert_eq!(
        fs::read(outcome.workspace_root.join("file")).unwrap(),
        b"isolated\n"
    );
    assert!(!outcome.workspace_root.join("secret.txt").exists());
    assert!(!outcome.workspace_root.join("once-large.bin").exists());
    assert!(outcome.checkpoint_count >= 4);

    let store = Store::open_read_only(&store_root).unwrap();
    let timeline = store.load_timeline(outcome.run_id, None, 1_000).unwrap();
    let mut output = Vec::new();
    let mut saw_marker = false;
    let mut isolated_with = None;
    let mut enriched_root = false;
    for event in timeline.events {
        match event.payload {
            EventPayload::TerminalOutput { object_id, .. } => {
                output.extend(
                    store
                        .load_object(object_id, rewind_capture::MAX_TERMINAL_CHUNK_BYTES as u64)
                        .unwrap(),
                );
            }
            EventPayload::MarkerCreated { .. } => saw_marker = true,
            EventPayload::WorkspaceIsolated { strategy } => isolated_with = Some(strategy),
            EventPayload::ProcessObserved { process }
                if process.relationship == ProcessRelationship::Root
                    && (process.parent_process_id.is_some() || process.executable.is_some()) =>
            {
                enriched_root = true;
            }
            _ => {}
        }
    }
    assert!(saw_marker);
    assert!(enriched_root);
    assert_eq!(output, b"hello");
    let expected_strategy = match outcome.clone_strategy {
        CloneStrategy::ApfsClone => WorkspaceCloneStrategy::ApfsClone,
        CloneStrategy::LinuxReflink => WorkspaceCloneStrategy::LinuxReflink,
        CloneStrategy::Mixed => WorkspaceCloneStrategy::Mixed,
        CloneStrategy::RecursiveCopy => WorkspaceCloneStrategy::RecursiveCopy,
    };
    assert_eq!(isolated_with, Some(expected_strategy));
    let warnings = store.load_warnings(outcome.run_id).unwrap();
    assert_eq!(
        warnings
            .iter()
            .filter(|warning| warning.code == "clone_fallback")
            .count(),
        usize::from(matches!(
            outcome.clone_strategy,
            CloneStrategy::Mixed | CloneStrategy::RecursiveCopy
        ))
    );
    assert_eq!(
        store
            .load_comparison_input(outcome.run_id)
            .unwrap()
            .warning_count,
        warnings.len() as u64
    );
    assert!(
        !store
            .object_exists(ObjectId::digest(ignored_secret))
            .unwrap()
    );
    assert!(
        !store
            .object_exists(ObjectId::digest(initially_large))
            .unwrap()
    );
    let final_snapshot = store.load_snapshot(outcome.final_snapshot).unwrap();
    assert!(
        final_snapshot
            .manifest
            .entries()
            .iter()
            .any(|entry| entry.path.as_str() == "once-large.bin")
    );
    assert!(
        final_snapshot
            .manifest
            .entries()
            .iter()
            .all(|entry| entry.path.as_str() != "secret.txt")
    );
    assert!(
        store
            .load_checkpoints(outcome.run_id)
            .unwrap()
            .iter()
            .any(|checkpoint| checkpoint.reason == CheckpointReason::FilesystemQuiescence)
    );
}

#[test]
fn root_exit_with_inherited_pty_is_bounded_and_terminates_the_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store_root = temp.path().join("store");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"unchanged\n").unwrap();

    let mut request = CaptureRequest::new(&source, std::env::current_exe().unwrap());
    request.arguments = vec![
        OsString::from("--exact"),
        OsString::from("pty_holder_helper"),
        OsString::from("--nocapture"),
    ];
    request
        .extra_environment
        .push((OsString::from("REWIND_PTY_HOLDER"), OsString::from("1")));
    request.options.process_poll_interval = Duration::from_millis(20);

    let started = Instant::now();
    let outcome = capture(&store_root, request).unwrap();
    let elapsed = started.elapsed();

    assert!(elapsed < Duration::from_secs(8), "capture took {elapsed:?}");
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.exit_status, ProcessExitStatus::Code(0));

    let store = Store::open_read_only(&store_root).unwrap();
    let warnings = store.load_warnings(outcome.run_id).unwrap();
    let forced_termination = warnings.iter().any(|warning| {
        warning.code == "other"
            && (warning
                .message
                .contains("root process exited while the PTY remained open")
                || warning
                    .message
                    .contains("terminated them before the final workspace scan"))
    });
    if cfg!(target_os = "linux") {
        assert!(forced_termination);
    }
    fs::write(outcome.workspace_root.join(".rewind-pty-holder-stop"), b"").unwrap();
    if !forced_termination {
        let done = outcome.workspace_root.join(".rewind-pty-holder-done");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(done.exists(), "PTY-holding descendant did not stop");
    }
    assert!(
        warnings
            .iter()
            .all(|warning| { !warning.message.contains("terminal tail may be incomplete") })
    );
}

#[test]
fn root_exit_terminates_descendants_that_closed_the_pty_before_final_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store_root = temp.path().join("store");
    let ready = temp.path().join("detached-ready");
    let release = temp.path().join("detached-release");
    let pid_file = temp.path().join("detached-pid");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"unchanged\n").unwrap();

    let mut request = CaptureRequest::new(&source, "/bin/sh");
    request.arguments = vec![
        OsString::from("-c"),
        OsString::from(
            "(trap '' HUP TERM INT; : > \"$REWIND_TEST_READY\"; while [ ! -e \"$REWIND_TEST_RELEASE\" ]; do sleep 0.01; done; printf late > late.txt) </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > \"$REWIND_TEST_PID\"; i=0; while [ ! -e \"$REWIND_TEST_READY\" ] && [ \"$i\" -lt 200 ]; do sleep 0.01; i=$((i + 1)); done; [ -e \"$REWIND_TEST_READY\" ] || exit 89",
        ),
    ];
    request.extra_environment.extend([
        (OsString::from("REWIND_TEST_READY"), ready.into_os_string()),
        (
            OsString::from("REWIND_TEST_RELEASE"),
            release.clone().into_os_string(),
        ),
        (
            OsString::from("REWIND_TEST_PID"),
            pid_file.clone().into_os_string(),
        ),
    ]);
    request.options.channel_capacity = 1;
    request.options.process_poll_interval = Duration::from_millis(20);

    let outcome = capture(&store_root, request).unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.exit_status, ProcessExitStatus::Code(0));
    let pid = fs::read_to_string(pid_file).unwrap();
    let descendant_alive = ProcessCommand::new("/bin/kill")
        .args(["-0", pid.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    if descendant_alive {
        fs::write(release, b"").unwrap();
    }
    assert!(!descendant_alive, "detached descendant survived capture");
    assert!(!outcome.workspace_root.join("late.txt").exists());

    let store = Store::open_read_only(&store_root).unwrap();
    assert!(
        store
            .load_snapshot(outcome.final_snapshot)
            .unwrap()
            .manifest
            .entries()
            .iter()
            .all(|entry| entry.path.as_str() != "late.txt")
    );
    assert!(
        store
            .load_warnings(outcome.run_id)
            .unwrap()
            .iter()
            .any(|warning| {
                warning
                    .message
                    .contains("terminated them before the final workspace scan")
            })
    );
}

#[test]
#[allow(clippy::zombie_processes)] // The orphaned descendant is the behavior under test.
fn pty_holder_helper() {
    if std::env::var_os("REWIND_PTY_HOLDER").is_none() {
        return;
    }
    let ready = ".rewind-pty-holder-ready";
    let _ = fs::remove_file(ready);
    let terminal = fs::OpenOptions::new().write(true).open("/dev/tty").unwrap();
    let mut child = ProcessCommand::new("/bin/sh")
        .args([
            "-c",
            "trap '' HUP INT TERM; : > .rewind-pty-holder-ready; while [ ! -e .rewind-pty-holder-stop ]; do /bin/sleep 0.05; done; : > .rewind-pty-holder-done",
        ])
        .stdout(terminal.try_clone().unwrap())
        .stderr(terminal)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !std::path::Path::new(ready).exists() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("PTY-holding descendant did not become ready");
        }
        thread::sleep(Duration::from_millis(10));
    }
    std::mem::forget(child);
}

#[test]
fn run_budget_counts_unique_objects_and_rejects_new_references() {
    let temp = tempfile::tempdir().unwrap();
    let shared_source = temp.path().join("shared-source");
    let shared_store = temp.path().join("shared-store");
    fs::create_dir(&shared_source).unwrap();
    fs::write(shared_source.join("file"), b"same").unwrap();
    let mut shared = CaptureRequest::new(&shared_source, "/bin/sh");
    shared.arguments = vec![OsString::from("-c"), OsString::from("printf 'same'")];
    shared.options.max_run_bytes = 4;

    let outcome = capture(&shared_store, shared).unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    let store = Store::open_read_only(&shared_store).unwrap();
    assert!(
        store
            .load_timeline(outcome.run_id, None, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TerminalOutput { .. }))
    );
    drop(store);

    let limited_source = temp.path().join("limited-source");
    let limited_store = temp.path().join("limited-store");
    fs::create_dir(&limited_source).unwrap();
    fs::write(limited_source.join("file"), b"same").unwrap();
    let mut limited = CaptureRequest::new(&limited_source, "/bin/sh");
    limited.arguments = vec![OsString::from("-c"), OsString::from("printf 'different'")];
    limited.options.max_run_bytes = 4;

    assert!(matches!(
        capture(&limited_store, limited),
        Err(CaptureError::RunStorageLimit { maximum: 4 })
    ));
    let store = Store::open_read_only(&limited_store).unwrap();
    let runs = store.list_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Failed);
    assert!(
        store
            .load_timeline(runs[0].id, None, 100)
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::TerminalOutput { .. }))
    );
    assert!(
        store
            .load_warnings(runs[0].id)
            .unwrap()
            .iter()
            .any(|warning| warning.code == "storage_limit")
    );
    drop(store);

    let snapshot_source = temp.path().join("snapshot-source");
    let snapshot_store = temp.path().join("snapshot-store");
    fs::create_dir(&snapshot_source).unwrap();
    fs::write(snapshot_source.join("file"), b"above").unwrap();
    let mut snapshot_limited = CaptureRequest::new(&snapshot_source, "/bin/true");
    snapshot_limited.options.max_run_bytes = 4;

    assert!(matches!(
        capture(&snapshot_store, snapshot_limited),
        Err(CaptureError::RunStorageLimit { maximum: 4 })
    ));
    let store = Store::open_read_only(&snapshot_store).unwrap();
    let runs = store.list_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Failed);
    assert!(!runs[0].workspace_root.exists());
    assert!(store.load_checkpoints(runs[0].id).unwrap().is_empty());
    assert!(
        store
            .load_timeline(runs[0].id, None, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(
                &event.payload,
                EventPayload::CheckpointFailed { failure, .. }
                    if failure.kind == RecorderFailureKind::ResourceLimit
            ))
    );
}
