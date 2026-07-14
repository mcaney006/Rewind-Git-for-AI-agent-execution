use std::fs;

use rewind_domain::{
    BranchId, CapturePolicy, Checkpoint, CheckpointId, CheckpointReason, Event, EventPayload,
    EventSequence, MonotonicDuration, ObjectId, Platform, ProcessExitStatus, ProcessId,
    RecorderWarning, RecorderWarningCode, Run, RunId, RunParent, RunStatus, Snapshot,
    SnapshotEntry, SnapshotEntryKind, SnapshotId, SnapshotManifest, TerminalStreamId, Timestamp,
    UnixPermissions,
};
use rewind_store::{RunFinish, Store, StoreError};

fn preparing_run(workspace: &std::path::Path) -> Run {
    Run {
        id: RunId::generate(),
        branch_id: BranchId::generate(),
        parent: None,
        command: "fake-agent".to_owned(),
        arguments: vec!["--fixture".to_owned()],
        workspace_root: workspace.to_path_buf(),
        started_at: Timestamp::from_unix_milliseconds(1),
        finished_at: None,
        monotonic_duration: None,
        status: RunStatus::Preparing,
        platform: Platform::MacOsAarch64,
        capture_policy: CapturePolicy::default(),
        initial_snapshot: None,
        final_snapshot: None,
        exit_status: None,
    }
}

fn snapshot(object_id: ObjectId, bytes: &[u8]) -> Snapshot {
    let manifest = SnapshotManifest::new(vec![SnapshotEntry {
        path: "file.txt".parse().unwrap(),
        kind: SnapshotEntryKind::File {
            object_id,
            size: bytes.len() as u64,
            executable: false,
        },
        permissions: UnixPermissions::new(0o644).unwrap(),
    }])
    .unwrap();
    let id = SnapshotId::digest(&serde_json::to_vec(&manifest).unwrap());
    Snapshot { id, manifest }
}

fn initial_checkpoint(run_id: RunId, snapshot_id: SnapshotId) -> Checkpoint {
    Checkpoint {
        id: CheckpointId::generate(),
        run_id,
        sequence: EventSequence::FIRST,
        label: None,
        reason: CheckpointReason::Initial,
        snapshot_id,
        created_at: Timestamp::from_unix_milliseconds(2),
        monotonic_offset: MonotonicDuration::ZERO,
    }
}

#[test]
fn objects_are_idempotent_verified_and_never_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();

    let first = store.put_object(b"same bytes", 1).unwrap();
    let second = store.put_object(b"same bytes", 2).unwrap();
    assert_eq!(first, second);
    assert_eq!(store.load_object(first.id, 64).unwrap(), b"same bytes");
    assert_eq!(
        first.id,
        ObjectId::digest(b"same bytes"),
        "identity is over logical bytes, not the envelope"
    );
    assert_eq!(
        store.objects().path_for(first.id),
        directory
            .path()
            .join("objects")
            .join(&first.id.to_string()[..2])
            .join(&first.id.to_string()[2..])
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(store.objects().path_for(first.id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory.path().join("metadata.sqlite3"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let wrong = ObjectId::digest(b"different");
    assert!(matches!(
        store.import_object(wrong, b"same bytes", 3),
        Err(StoreError::ObjectDigestMismatch { .. })
    ));

    let orphan = store.put_object(b"orphan", 3).unwrap();
    let page = store.list_objects(None, 10).unwrap();
    assert_eq!(page.objects.len(), 2);
    assert!(!page.has_more);
    assert!(
        store
            .sample_object_corruption(10, 1024)
            .unwrap()
            .corrupt
            .is_empty()
    );
    let budgeted = store.sample_object_corruption(10, 0).unwrap();
    assert_eq!(budgeted.checked, 0);
    assert_eq!(budgeted.skipped.len(), 2);
    assert!(budgeted.corrupt.is_empty());
    let deleted = store.delete_object(orphan.id).unwrap();
    assert_eq!(deleted.id, orphan.id);
    assert!(!store.object_exists(orphan.id).unwrap());

    fs::write(store.objects().path_for(first.id), b"corrupt").unwrap();
    assert!(store.load_object(first.id, 64).is_err());
    assert!(store.put_object(b"same bytes", 4).is_err());
    assert_eq!(
        fs::read(store.objects().path_for(first.id)).unwrap(),
        b"corrupt",
        "a corrupt existing path must not be overwritten"
    );
    let verification = store.sample_object_corruption(10, 1024).unwrap();
    assert_eq!(verification.checked, 1);
    assert_eq!(verification.corrupt.len(), 1);
}

#[test]
fn gc_references_fail_closed_when_event_kind_disagrees_with_payload() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let store_path = directory.path().join("store");
    fs::create_dir(&workspace).unwrap();
    let mut store = Store::open(&store_path).unwrap();
    let run = preparing_run(&workspace);
    store.create_run(&run).unwrap();
    let object = store.put_object(b"sensitive input", 1).unwrap();
    let event = Event::new(
        run.id,
        EventSequence::FIRST,
        Timestamp::from_unix_milliseconds(2),
        MonotonicDuration::ZERO,
        EventPayload::TerminalInput {
            stream_id: TerminalStreamId::generate(),
            object_id: object.id,
            byte_len: object.logical_size,
        },
    )
    .unwrap();
    store.append_event_batch(&[event]).unwrap();
    let object_path = store.objects().path_for(object.id);
    drop(store);

    let connection = rusqlite::Connection::open(store_path.join("metadata.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET kind = 'recorder_warning' WHERE run_id = ?1",
            [run.id.to_string()],
        )
        .unwrap();
    drop(connection);

    let mut store = Store::open(&store_path).unwrap();
    assert!(matches!(
        store.validate_gc_references(),
        Err(StoreError::Invariant { .. })
    ));
    assert_eq!(
        store.list_referenced_object_ids(None, 10).unwrap().ids,
        vec![object.id],
        "reachability follows the typed payload even when the index is corrupt"
    );
    assert!(matches!(
        store.delete_object(object.id),
        Err(StoreError::Invariant { .. })
    ));
    assert!(store.object_exists(object.id).unwrap());
    assert!(object_path.is_file());
}

#[test]
fn writer_lock_is_diagnostic_and_readers_can_open_during_writes() {
    let directory = tempfile::tempdir().unwrap();
    let writer = Store::open(directory.path()).unwrap();
    let error = match Store::open(directory.path()) {
        Ok(_) => panic!("second writer acquired the lock"),
        Err(error) => error,
    };
    match error {
        StoreError::Locked { owner, .. } => {
            assert_eq!(owner.pid, Some(std::process::id()));
            assert!(owner.token.is_some());
        }
        other => panic!("unexpected error: {other}"),
    }
    let reader = Store::open_read_only(directory.path()).unwrap();
    assert!(reader.is_read_only());
    drop(reader);
    drop(writer);
    assert!(directory.path().join("writer.lock").is_file());
    assert!(
        Store::inspect_writer_lock(directory.path())
            .unwrap()
            .is_none()
    );
    Store::open(directory.path()).unwrap();
}

#[test]
fn snapshot_checkpoint_events_and_startup_recovery_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut store = Store::open(directory.path().join("store")).unwrap();
    let run = preparing_run(&workspace);
    store.create_run(&run).unwrap();

    let bytes = b"hello\n";
    let object = store.put_object(bytes, 1).unwrap();
    let snapshot = snapshot(object.id, bytes);
    let checkpoint = initial_checkpoint(run.id, snapshot.id);
    let committed = Event::new(
        run.id,
        EventSequence::FIRST,
        checkpoint.created_at,
        checkpoint.monotonic_offset,
        EventPayload::CheckpointCommitted {
            checkpoint_id: checkpoint.id,
            snapshot_id: snapshot.id,
        },
    )
    .unwrap();
    store
        .commit_checkpoint_with_event(&checkpoint, &snapshot, &committed)
        .unwrap();
    store.mark_run_running(run.id, snapshot.id).unwrap();

    let stream_id = TerminalStreamId::generate();
    let started = Event::new(
        run.id,
        EventSequence::new(2).unwrap(),
        Timestamp::from_unix_milliseconds(3),
        MonotonicDuration::from_nanoseconds(10),
        EventPayload::RunStarted {
            root_process_id: ProcessId::new(42).unwrap(),
            terminal_stream_id: stream_id,
        },
    )
    .unwrap();
    let output = Event::new(
        run.id,
        EventSequence::new(3).unwrap(),
        Timestamp::from_unix_milliseconds(2),
        MonotonicDuration::from_nanoseconds(20),
        EventPayload::TerminalOutput {
            stream_id,
            object_id: object.id,
            byte_len: bytes.len() as u64,
        },
    )
    .unwrap();
    store
        .append_event_batch(&[started.clone(), output.clone()])
        .unwrap();

    let page = store.load_timeline(run.id, None, 1).unwrap();
    assert_eq!(page.events, vec![committed]);
    assert!(page.has_more);
    let page = store
        .load_timeline(run.id, Some(EventSequence::FIRST), 10)
        .unwrap();
    assert_eq!(page.events, vec![started, output]);
    assert!(!page.has_more);
    assert_eq!(store.load_snapshot(snapshot.id).unwrap(), snapshot);
    assert_eq!(store.load_checkpoint(checkpoint.id).unwrap(), checkpoint);
    assert_eq!(
        store.list_referenced_object_ids(None, 10).unwrap().ids,
        vec![object.id]
    );
    assert_eq!(
        store.list_run_object_ids(run.id, None, 10).unwrap().ids,
        vec![object.id]
    );
    assert!(matches!(
        store.delete_object(object.id),
        Err(StoreError::ObjectReferenced { .. })
    ));

    let mut child = preparing_run(&workspace);
    child.parent = Some(RunParent {
        run_id: run.id,
        checkpoint_id: checkpoint.id,
    });
    store.create_run(&child).unwrap();
    assert_eq!(store.load_run(child.id).unwrap().parent, child.parent);

    let comparison = store.load_comparison_input(run.id).unwrap();
    assert_eq!(comparison.checkpoint_count, 1);
    assert_eq!(comparison.terminal_output_bytes, bytes.len() as u64);
    drop(store);

    let store = Store::open(directory.path().join("store")).unwrap();
    let recovered = store.load_run(run.id).unwrap();
    assert_eq!(recovered.status, RunStatus::Interrupted);
    assert_eq!(
        recovered.monotonic_duration,
        Some(MonotonicDuration::from_nanoseconds(20))
    );
    assert!(recovered.finished_at.is_some());
    let warnings = store.load_warnings(run.id).unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "incomplete_run_recovered");
}

#[test]
fn discontinuous_event_batch_is_rejected_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut store = Store::open(directory.path().join("store")).unwrap();
    let run = preparing_run(&workspace);
    store.create_run(&run).unwrap();
    let stream_id = TerminalStreamId::generate();
    let first = Event::new(
        run.id,
        EventSequence::FIRST,
        Timestamp::from_unix_milliseconds(1),
        MonotonicDuration::ZERO,
        EventPayload::RunStarted {
            root_process_id: ProcessId::new(1).unwrap(),
            terminal_stream_id: stream_id,
        },
    )
    .unwrap();
    let third = Event::new(
        run.id,
        EventSequence::new(3).unwrap(),
        Timestamp::from_unix_milliseconds(1),
        MonotonicDuration::from_nanoseconds(1),
        EventPayload::TerminalResized {
            stream_id,
            columns: 80,
            rows: 24,
        },
    )
    .unwrap();
    assert!(matches!(
        store.append_event_batch(&[first, third]),
        Err(StoreError::EventOrder { .. })
    ));
    assert!(
        store
            .load_timeline(run.id, None, 10)
            .unwrap()
            .events
            .is_empty()
    );
}

#[test]
fn recorder_warning_events_are_indexed_for_comparison() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut store = Store::open(directory.path().join("store")).unwrap();
    let run = preparing_run(&workspace);
    store.create_run(&run).unwrap();
    let warning = Event::new(
        run.id,
        EventSequence::FIRST,
        Timestamp::from_unix_milliseconds(4),
        MonotonicDuration::ZERO,
        EventPayload::RecorderWarning {
            warning: RecorderWarning {
                code: RecorderWarningCode::FilesystemRace,
                message: "Workspace changed during an authoritative scan.".to_owned(),
            },
        },
    )
    .unwrap();

    store.append_event_batch(&[warning]).unwrap();

    let warnings = store.load_warnings(run.id).unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].sequence, 1);
    assert_eq!(warnings[0].code, "filesystem_race");
    assert_eq!(warnings[0].created_at, Timestamp::from_unix_milliseconds(4));
    assert_eq!(
        store.load_comparison_input(run.id).unwrap().warning_count,
        1
    );
}

#[test]
fn finish_and_list_preserve_terminal_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut store = Store::open(directory.path().join("store")).unwrap();
    let run = preparing_run(&workspace);
    store.create_run(&run).unwrap();
    store
        .finish_run(
            run.id,
            RunFinish {
                status: RunStatus::Failed,
                finished_at: Timestamp::from_unix_milliseconds(9),
                monotonic_duration: MonotonicDuration::from_nanoseconds(8),
                final_snapshot: None,
                exit_status: Some(ProcessExitStatus::Code(7)),
            },
        )
        .unwrap();

    let loaded = store.load_run(run.id).unwrap();
    assert_eq!(loaded.status, RunStatus::Failed);
    assert_eq!(loaded.arguments, vec!["--fixture"]);
    assert_eq!(loaded.exit_status, Some(ProcessExitStatus::Code(7)));
    assert_eq!(store.list_runs().unwrap(), vec![loaded]);
    assert!(
        store
            .finish_run(
                run.id,
                RunFinish {
                    status: RunStatus::Failed,
                    finished_at: Timestamp::from_unix_milliseconds(10),
                    monotonic_duration: MonotonicDuration::from_nanoseconds(9),
                    final_snapshot: None,
                    exit_status: Some(ProcessExitStatus::Code(7)),
                },
            )
            .is_err()
    );
}

#[test]
fn terminal_event_and_run_transition_commit_or_roll_back_together() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut store = Store::open(directory.path().join("store")).unwrap();
    let run = preparing_run(&workspace);
    store.create_run(&run).unwrap();
    let terminal = Event::new(
        run.id,
        EventSequence::FIRST,
        Timestamp::from_unix_milliseconds(9),
        MonotonicDuration::from_nanoseconds(8),
        EventPayload::RunCompleted {
            status: RunStatus::Failed,
            exit_status: Some(ProcessExitStatus::Code(7)),
        },
    )
    .unwrap();
    let mut finish = RunFinish {
        status: RunStatus::Failed,
        finished_at: Timestamp::from_unix_milliseconds(9),
        monotonic_duration: MonotonicDuration::from_nanoseconds(8),
        final_snapshot: Some(SnapshotId::digest(b"missing")),
        exit_status: Some(ProcessExitStatus::Code(7)),
    };

    assert!(
        store
            .finish_run_with_events(run.id, finish, std::slice::from_ref(&terminal))
            .is_err()
    );
    assert_eq!(store.load_run(run.id).unwrap().status, RunStatus::Preparing);
    assert!(
        store
            .load_timeline(run.id, None, 10)
            .unwrap()
            .events
            .is_empty()
    );

    finish.final_snapshot = None;
    store
        .finish_run_with_events(run.id, finish, std::slice::from_ref(&terminal))
        .unwrap();
    assert_eq!(store.load_run(run.id).unwrap().status, RunStatus::Failed);
    assert_eq!(
        store.load_timeline(run.id, None, 10).unwrap().events,
        vec![terminal]
    );
}
