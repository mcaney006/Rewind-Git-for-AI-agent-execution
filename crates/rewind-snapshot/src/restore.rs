use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rewind_domain::{ObjectId, Snapshot, SnapshotEntryKind, SnapshotId, SnapshotPath};
use rewind_platform::{DirectoryChildState, DirectoryRoot, FileSystemError};
use rewind_store::Store;

use crate::{DEFAULT_MAX_FILE_SIZE, Result, SnapshotError, validate_manifest};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Safety and resource policy for one checkout materialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializeOptions {
    /// Replace an existing nonempty destination only when explicitly enabled.
    pub force: bool,
    /// Maximum verified bytes loaded from one object at a time.
    pub max_file_size: u64,
}

impl Default for MaterializeOptions {
    fn default() -> Self {
        Self {
            force: false,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// Metadata that cannot be reproduced through the portable safe Unix API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializeWarning {
    /// Symlink mode bits were recorded but are not applied because chmod would
    /// follow the link on supported standard-library APIs.
    SymlinkPermissionsNotRestored {
        /// Materialized link path.
        path: SnapshotPath,
        /// Captured mode bits.
        captured: u16,
    },
}

/// Evidence from a completed atomic checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializeReport {
    /// Installed destination.
    pub destination: PathBuf,
    /// Regular files written.
    pub files: u64,
    /// Explicit directories written, excluding the checkout root.
    pub directories: u64,
    /// Symbolic links recreated last.
    pub symlinks: u64,
    /// Logical regular-file bytes written.
    pub logical_bytes: u64,
    /// Visible unsupported metadata notes.
    pub warnings: Vec<MaterializeWarning>,
}

/// Preflights every path and object, builds a private sibling tree, then renames it into place.
pub fn materialize(
    snapshot: &Snapshot,
    store: &Store,
    destination: impl AsRef<Path>,
    options: &MaterializeOptions,
) -> Result<MaterializeReport> {
    let destination = destination.as_ref();
    validate_snapshot(snapshot)?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let destination_name = destination
        .file_name()
        .ok_or_else(|| SnapshotError::InvalidDestination(destination.to_path_buf()))?;
    let parent = DirectoryRoot::open(parent)?;
    let destination_state = parent.child_state(destination_name)?;
    if destination_state == DirectoryChildState::Occupied && !options.force {
        return Err(SnapshotError::DestinationNotEmpty(
            destination.to_path_buf(),
        ));
    }
    preflight_objects(snapshot, store, options.max_file_size)?;

    let mut temporary = TemporaryTree::create(&parent, "tmp", destination_name, destination)?;
    let mut report = build_tree(snapshot, store, temporary.root(), options.max_file_size)?;
    temporary.root().sync()?;

    if !options.force {
        let current = parent.child_state(destination_name)?;
        if current == DirectoryChildState::Occupied
            || (destination_state == DirectoryChildState::Absent
                && current != DirectoryChildState::Absent)
        {
            return Err(SnapshotError::DestinationNotEmpty(
                destination.to_path_buf(),
            ));
        }
    }

    if parent.child_state(destination_name)? == DirectoryChildState::Absent {
        parent.rename_child(temporary.name(), destination_name)?;
        temporary.disarm();
    } else {
        install_over_existing(&parent, &mut temporary, destination_name, destination)?;
    }
    parent.sync()?;
    report.destination = destination.to_path_buf();
    Ok(report)
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    validate_manifest(&snapshot.manifest)?;
    let canonical =
        serde_json::to_vec(&snapshot.manifest).map_err(SnapshotError::CanonicalEncoding)?;
    let actual = SnapshotId::digest(&canonical);
    if actual == snapshot.id {
        Ok(())
    } else {
        Err(SnapshotError::SnapshotIdentityMismatch {
            expected: snapshot.id,
            actual,
        })
    }
}

fn preflight_objects(snapshot: &Snapshot, store: &Store, maximum: u64) -> Result<()> {
    let mut objects = BTreeMap::<ObjectId, (u64, SnapshotPath)>::new();
    for entry in snapshot.manifest.entries() {
        let SnapshotEntryKind::File {
            object_id, size, ..
        } = entry.kind
        else {
            continue;
        };
        if size > maximum {
            return Err(SnapshotError::RestoreFileTooLarge {
                path: entry.path.clone(),
                actual: size,
                maximum,
            });
        }
        if let Some((expected, first_path)) = objects.get(&object_id) {
            if *expected != size {
                return Err(SnapshotError::ObjectSizeMismatch {
                    path: entry.path.clone(),
                    expected: size,
                    actual: *expected,
                });
            }
            let _ = first_path;
            continue;
        }
        objects.insert(object_id, (size, entry.path.clone()));
    }

    for (object_id, (expected, path)) in objects {
        let bytes = store.load_object(object_id, expected)?;
        let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if actual != expected {
            return Err(SnapshotError::ObjectSizeMismatch {
                path,
                expected,
                actual,
            });
        }
    }
    Ok(())
}

fn build_tree(
    snapshot: &Snapshot,
    store: &Store,
    root: &DirectoryRoot,
    maximum: u64,
) -> Result<MaterializeReport> {
    let mut report = MaterializeReport {
        destination: PathBuf::new(),
        files: 0,
        directories: 0,
        symlinks: 0,
        logical_bytes: 0,
        warnings: Vec::new(),
    };

    for entry in snapshot.manifest.entries() {
        if matches!(entry.kind, SnapshotEntryKind::Directory) {
            root.create_directory_beneath(entry.path.as_str())?;
            report.directories += 1;
        }
    }

    for entry in snapshot.manifest.entries() {
        let SnapshotEntryKind::File {
            object_id, size, ..
        } = entry.kind
        else {
            continue;
        };
        if size > maximum {
            return Err(SnapshotError::RestoreFileTooLarge {
                path: entry.path.clone(),
                actual: size,
                maximum,
            });
        }
        let bytes = store.load_object(object_id, size)?;
        let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if actual != size {
            return Err(SnapshotError::ObjectSizeMismatch {
                path: entry.path.clone(),
                expected: size,
                actual,
            });
        }
        root.create_file_beneath(
            entry.path.as_str(),
            &bytes,
            u32::from(entry.permissions.bits()),
        )?;
        report.files += 1;
        report.logical_bytes = report.logical_bytes.checked_add(size).ok_or_else(|| {
            SnapshotError::ObjectSizeMismatch {
                path: entry.path.clone(),
                expected: size,
                actual: u64::MAX,
            }
        })?;
    }

    for entry in snapshot.manifest.entries() {
        let SnapshotEntryKind::Symlink { target } = &entry.kind else {
            continue;
        };
        root.create_symlink_beneath(entry.path.as_str(), OsStr::new(target))?;
        report.symlinks += 1;
        report
            .warnings
            .push(MaterializeWarning::SymlinkPermissionsNotRestored {
                path: entry.path.clone(),
                captured: entry.permissions.bits(),
            });
    }

    for entry in snapshot.manifest.entries().iter().rev() {
        if matches!(entry.kind, SnapshotEntryKind::Directory) {
            root.finish_directory_beneath(
                entry.path.as_str(),
                u32::from(entry.permissions.bits()),
            )?;
        }
    }
    Ok(report)
}

fn install_over_existing(
    parent: &DirectoryRoot,
    temporary: &mut TemporaryTree<'_>,
    destination_name: &OsStr,
    destination: &Path,
) -> Result<()> {
    let backup = unique_available_name(parent, "old", destination_name, destination)?;
    parent.rename_child(destination_name, &backup)?;
    match parent.rename_child(temporary.name(), destination_name) {
        Ok(()) => {
            temporary.disarm();
            parent.remove_child(&backup)?;
            Ok(())
        }
        Err(install) => match parent.rename_child(&backup, destination_name) {
            Ok(()) => Err(install.into()),
            Err(rollback) => Err(SnapshotError::InstallRollback {
                destination: destination.to_path_buf(),
                install: Box::new(install),
                rollback: Box::new(rollback),
            }),
        },
    }
}

struct TemporaryTree<'parent> {
    parent: &'parent DirectoryRoot,
    name: OsString,
    root: DirectoryRoot,
    active: bool,
}

impl<'parent> TemporaryTree<'parent> {
    fn create(
        parent: &'parent DirectoryRoot,
        label: &str,
        destination_name: &OsStr,
        destination: &Path,
    ) -> Result<Self> {
        for _ in 0..64 {
            let name = unique_name(label, destination_name);
            match parent.create_child_directory(&name) {
                Ok(root) => {
                    return Ok(Self {
                        parent,
                        name,
                        root,
                        active: true,
                    });
                }
                Err(FileSystemError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(SnapshotError::TemporaryNameExhausted(
            destination.to_path_buf(),
        ))
    }

    fn root(&self) -> &DirectoryRoot {
        &self.root
    }

    fn name(&self) -> &OsStr {
        &self.name
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TemporaryTree<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.parent.remove_child(&self.name);
        }
    }
}

fn unique_available_name(
    parent: &DirectoryRoot,
    label: &str,
    destination_name: &OsStr,
    destination: &Path,
) -> Result<OsString> {
    for _ in 0..64 {
        let candidate = unique_name(label, destination_name);
        if parent.child_state(&candidate)? == DirectoryChildState::Absent {
            return Ok(candidate);
        }
    }
    Err(SnapshotError::TemporaryNameExhausted(
        destination.to_path_buf(),
    ))
}

fn unique_name(label: &str, destination_name: &OsStr) -> OsString {
    let name = destination_name.to_string_lossy();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!(
        ".rewind-{name}.{label}-{}-{sequence}",
        std::process::id()
    ))
}
