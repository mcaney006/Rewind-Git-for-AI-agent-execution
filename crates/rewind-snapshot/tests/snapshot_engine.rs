use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use rewind_domain::{
    ObjectId, Snapshot, SnapshotEntry, SnapshotEntryKind, SnapshotId, SnapshotManifest,
    UnixPermissions,
};
use rewind_platform::FileSystemError;
use rewind_snapshot::{
    EntryChange, ExclusionReason, IgnorePattern, MaterializeOptions, ScanOptions, SnapshotError,
    diff_snapshots, materialize, scan_workspace, validate_manifest, validate_relative_path,
};
use rewind_store::Store;

fn open_store(root: &std::path::Path) -> Store {
    Store::open(root).unwrap()
}

fn write_tree(root: &std::path::Path, reverse: bool) {
    fs::create_dir(root).unwrap();
    let paths = if reverse {
        ["z.txt", "nested/tool"]
    } else {
        ["nested/tool", "z.txt"]
    };
    for path in paths {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(
            &path,
            if path.ends_with("tool") {
                b"tool"
            } else {
                b"zeta"
            },
        )
        .unwrap();
    }
    fs::set_permissions(root.join("nested"), fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(root.join("nested/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("nested/tool", root.join("link")).unwrap();
}

#[test]
fn scan_identity_is_independent_of_creation_order_and_reuses_objects() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    write_tree(&first, false);
    write_tree(&second, true);
    let mut store = open_store(&temp.path().join("store"));

    let first_scan = scan_workspace(&first, &mut store, &ScanOptions::default(), 1).unwrap();
    let second_scan = scan_workspace(&second, &mut store, &ScanOptions::default(), 2).unwrap();
    let repeated = scan_workspace(&first, &mut store, &ScanOptions::default(), 3).unwrap();

    assert_eq!(first_scan.snapshot, second_scan.snapshot);
    assert_eq!(first_scan.snapshot, repeated.snapshot);
    let tool = first_scan
        .snapshot
        .manifest
        .entries()
        .iter()
        .find(|entry| entry.path.as_str() == "nested/tool")
        .unwrap();
    let SnapshotEntryKind::File { object_id, .. } = tool.kind else {
        panic!("tool should be a file")
    };
    assert!(store.object_exists(object_id).unwrap());
}

#[test]
fn restore_reproduces_supported_tree_and_reports_symlink_mode() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("checkout");
    write_tree(&source, false);
    let mut store = open_store(&temp.path().join("store"));
    let original = scan_workspace(&source, &mut store, &ScanOptions::default(), 1).unwrap();

    let report = materialize(
        &original.snapshot,
        &store,
        &destination,
        &MaterializeOptions::default(),
    )
    .unwrap();
    let restored = scan_workspace(&destination, &mut store, &ScanOptions::default(), 2).unwrap();

    assert_eq!(original.snapshot, restored.snapshot);
    assert_eq!(
        fs::read_link(destination.join("link")).unwrap(),
        std::path::Path::new("nested/tool")
    );
    assert_eq!(
        fs::metadata(destination.join("nested/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    assert_eq!(report.files, 2);
    assert_eq!(report.symlinks, 1);
    assert_eq!(report.warnings.len(), 1);
}

#[test]
fn restore_flushes_restrictive_directories_through_pinned_descriptors() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("checkout");
    fs::create_dir_all(source.join("restricted")).unwrap();
    let mut store = open_store(&temp.path().join("store"));
    let scanned = scan_workspace(&source, &mut store, &ScanOptions::default(), 1).unwrap();
    let entries = scanned
        .snapshot
        .manifest
        .entries()
        .iter()
        .cloned()
        .map(|mut entry| {
            if entry.path.as_str() == "restricted" {
                entry.permissions = UnixPermissions::new(0).unwrap();
            }
            entry
        })
        .collect();
    let manifest = SnapshotManifest::new(entries).unwrap();
    let canonical = serde_json::to_vec(&manifest).unwrap();
    let snapshot = Snapshot {
        id: SnapshotId::digest(&canonical),
        manifest,
    };

    materialize(
        &snapshot,
        &store,
        &destination,
        &MaterializeOptions::default(),
    )
    .unwrap();

    assert_eq!(
        fs::symlink_metadata(destination.join("restricted"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0
    );
}

#[test]
fn diff_is_complete_sorted_and_symmetric() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("changed"), b"before").unwrap();
    fs::write(workspace.join("removed"), b"gone").unwrap();
    fs::write(workspace.join("mode"), b"same").unwrap();
    let mut store = open_store(&temp.path().join("store"));
    let before = scan_workspace(&workspace, &mut store, &ScanOptions::default(), 1)
        .unwrap()
        .snapshot;

    fs::write(workspace.join("changed"), b"after").unwrap();
    fs::remove_file(workspace.join("removed")).unwrap();
    fs::write(workspace.join("added"), b"new").unwrap();
    fs::set_permissions(workspace.join("mode"), fs::Permissions::from_mode(0o755)).unwrap();
    let after = scan_workspace(&workspace, &mut store, &ScanOptions::default(), 2)
        .unwrap()
        .snapshot;

    let forward = diff_snapshots(&before, &after);
    let backward = diff_snapshots(&after, &before);
    assert_eq!(forward.reversed(), backward);
    assert_eq!(forward.changes.len(), 4);
    assert!(
        forward
            .changes
            .windows(2)
            .all(|pair| pair[0].path() < pair[1].path())
    );
    assert!(forward.changes.iter().any(|change| matches!(change, EntryChange::Modified { before, after } if before.path.as_str() == "mode" && before.permissions != after.permissions)));
}

#[test]
fn ignores_and_size_limits_are_visible_without_storing_content() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("large"), b"12345").unwrap();
    fs::create_dir(workspace.join("private")).unwrap();
    fs::write(workspace.join("private/secret"), b"do-not-store").unwrap();
    let mut store = open_store(&temp.path().join("store"));
    let options = ScanOptions {
        ignore: vec!["private".parse::<IgnorePattern>().unwrap()],
        max_file_size: 4,
    };

    let report = scan_workspace(&workspace, &mut store, &options, 1).unwrap();
    assert!(report.snapshot.manifest.entries().is_empty());
    assert_eq!(report.exclusions.len(), 2);
    assert!(
        report
            .exclusions
            .iter()
            .any(|excluded| matches!(excluded.reason, ExclusionReason::Ignored { .. }))
    );
    assert!(report.exclusions.iter().any(|excluded| matches!(
        excluded.reason,
        ExclusionReason::FileTooLarge {
            actual: 5,
            maximum: 4
        }
    )));
    assert!(
        !store
            .object_exists(ObjectId::digest(b"do-not-store"))
            .unwrap()
    );
    assert!(!store.object_exists(ObjectId::digest(b"12345")).unwrap());
}

#[test]
fn raced_symlink_ancestor_never_stores_outside_file_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    let ancestor = workspace.join("ancestor");
    let parked = workspace.join("parked");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&ancestor).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(ancestor.join("victim"), b"inside").unwrap();
    let sentinel = b"outside-sentinel-must-not-be-stored";
    fs::write(outside.join("victim"), sentinel).unwrap();
    let mut store = open_store(&temp.path().join("store"));

    let stop = Arc::new(AtomicBool::new(false));
    let attacker_stop = Arc::clone(&stop);
    let attacker_outside = outside.clone();
    let attacker = thread::spawn(move || {
        while !attacker_stop.load(Ordering::Acquire) {
            fs::rename(&ancestor, &parked).unwrap();
            symlink(&attacker_outside, &ancestor).unwrap();
            thread::yield_now();
            fs::remove_file(&ancestor).unwrap();
            fs::rename(&parked, &ancestor).unwrap();
            thread::yield_now();
        }
    });

    for created_unix_ms in 1..=128 {
        let _ = scan_workspace(
            &workspace,
            &mut store,
            &ScanOptions::default(),
            created_unix_ms,
        );
    }
    stop.store(true, Ordering::Release);
    attacker.join().unwrap();

    assert!(!store.object_exists(ObjectId::digest(sentinel)).unwrap());
}

#[test]
fn raced_symlink_ancestor_never_commits_outside_names_targets_or_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let ancestor = workspace.join("ancestor");
    let parked = temp.path().join("parked");
    let outside = temp.path().join("outside");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&ancestor).unwrap();
    fs::create_dir(ancestor.join("inside-directory")).unwrap();
    fs::set_permissions(
        ancestor.join("inside-directory"),
        fs::Permissions::from_mode(0o710),
    )
    .unwrap();
    symlink("inside-target", ancestor.join("inside-link")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(outside.join("outside-name")).unwrap();
    fs::create_dir(outside.join("inside-directory")).unwrap();
    fs::set_permissions(
        outside.join("inside-directory"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    symlink("outside-target-sentinel", outside.join("inside-link")).unwrap();
    for index in 0..64 {
        fs::create_dir(workspace.join(format!("stable-{index:02}"))).unwrap();
    }
    let mut store = open_store(&temp.path().join("store"));

    let stop = Arc::new(AtomicBool::new(false));
    let attacker_stop = Arc::clone(&stop);
    let attacker_outside = outside.clone();
    let attacker = thread::spawn(move || {
        while !attacker_stop.load(Ordering::Acquire) {
            fs::rename(&ancestor, &parked).unwrap();
            symlink(&attacker_outside, &ancestor).unwrap();
            thread::yield_now();
            fs::remove_file(&ancestor).unwrap();
            fs::rename(&parked, &ancestor).unwrap();
            thread::yield_now();
        }
    });

    for created_unix_ms in 1..=64 {
        match scan_workspace(
            &workspace,
            &mut store,
            &ScanOptions::default(),
            created_unix_ms,
        ) {
            Ok(report) => {
                for entry in report.snapshot.manifest.entries() {
                    assert!(!entry.path.as_str().contains("outside"));
                    if entry.path.as_str() == "ancestor/inside-link"
                        && let SnapshotEntryKind::Symlink { target } = &entry.kind
                    {
                        assert_ne!(target, "outside-target-sentinel");
                        assert_ne!(target, outside.to_str().unwrap());
                    }
                    if entry.path.as_str() == "ancestor/inside-directory" {
                        assert_eq!(entry.permissions.bits(), 0o710);
                    }
                }
            }
            Err(SnapshotError::Platform(FileSystemError::ChangedDuringRead(_))) => {}
            Err(error) => panic!("concurrent swap returned an untyped error: {error}"),
        }
    }
    stop.store(true, Ordering::Release);
    attacker.join().unwrap();
}

#[test]
fn path_and_tree_preflight_reject_escape_case_aliases_and_bad_parents() {
    for malicious in [
        "",
        "/absolute",
        "../escape",
        "a/../b",
        "./a",
        "a//b",
        "a\\b",
        "a\0b",
    ] {
        assert!(
            validate_relative_path(malicious).is_err(),
            "accepted {malicious:?}"
        );
    }

    let upper = file_entry("Readme", b"a", 0o644);
    let lower = file_entry("README", b"b", 0o644);
    let collision = SnapshotManifest::new(vec![upper, lower]).unwrap();
    assert!(matches!(
        validate_manifest(&collision),
        Err(SnapshotError::CaseFoldCollision { .. })
    ));

    let missing = SnapshotManifest::new(vec![file_entry("missing/child", b"x", 0o644)]).unwrap();
    assert!(matches!(
        validate_manifest(&missing),
        Err(SnapshotError::MissingParent { .. })
    ));

    let non_directory = SnapshotManifest::new(vec![
        file_entry("parent", b"x", 0o644),
        file_entry("parent/child", b"y", 0o644),
    ])
    .unwrap();
    assert!(matches!(
        validate_manifest(&non_directory),
        Err(SnapshotError::ParentNotDirectory { .. })
    ));
}

#[test]
fn restore_preflights_corrupt_objects_and_requires_force_for_nonempty_destination() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("file"), b"content").unwrap();
    let mut store = open_store(&temp.path().join("store"));
    let snapshot = scan_workspace(&workspace, &mut store, &ScanOptions::default(), 1)
        .unwrap()
        .snapshot;
    let object_id = match snapshot.manifest.entries()[0].kind {
        SnapshotEntryKind::File { object_id, .. } => object_id,
        _ => panic!("expected file"),
    };

    let occupied = temp.path().join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("keep"), b"keep").unwrap();
    assert!(matches!(
        materialize(&snapshot, &store, &occupied, &MaterializeOptions::default()),
        Err(SnapshotError::DestinationNotEmpty(_))
    ));
    assert_eq!(fs::read(occupied.join("keep")).unwrap(), b"keep");
    materialize(
        &snapshot,
        &store,
        &occupied,
        &MaterializeOptions {
            force: true,
            ..MaterializeOptions::default()
        },
    )
    .unwrap();
    assert!(!occupied.join("keep").exists());
    assert_eq!(fs::read(occupied.join("file")).unwrap(), b"content");

    let corrupt_destination = temp.path().join("corrupt-checkout");
    fs::write(store.objects().path_for(object_id), b"corrupt").unwrap();
    assert!(
        materialize(
            &snapshot,
            &store,
            &corrupt_destination,
            &MaterializeOptions::default()
        )
        .is_err()
    );
    assert!(!corrupt_destination.exists());
}

fn file_entry(path: &str, bytes: &[u8], mode: u16) -> SnapshotEntry {
    let permissions = UnixPermissions::new(mode).unwrap();
    SnapshotEntry {
        path: path.parse().unwrap(),
        kind: SnapshotEntryKind::File {
            object_id: ObjectId::digest(bytes),
            size: u64::try_from(bytes.len()).unwrap(),
            executable: permissions.is_executable(),
        },
        permissions,
    }
}
