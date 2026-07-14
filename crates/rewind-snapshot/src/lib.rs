//! Deterministic workspace scanning, comparison, and safe materialization.

mod diff;
mod restore;
mod scan;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use diff::{EntryChange, SnapshotDiff, diff_snapshots};
pub use restore::{MaterializeOptions, MaterializeReport, MaterializeWarning, materialize};
use rewind_domain::{
    SnapshotId, SnapshotManifest, SnapshotManifestError, SnapshotPath, SnapshotPathError,
};
use rewind_platform::FileSystemError;
use rewind_store::StoreError;
pub use scan::{
    DEFAULT_MAX_FILE_SIZE, Exclusion, ExclusionReason, IgnorePattern, ScanOptions, ScanReport,
    scan_workspace,
};
use thiserror::Error;

/// A snapshot scan, comparison, or materialization failure.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// The configured workspace root is not a real directory.
    #[error("workspace root is not a directory: {0}")]
    RootNotDirectory(PathBuf),
    /// A filesystem name or symlink target cannot be represented durably.
    #[error("workspace path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    /// A relative path failed the durable path rules.
    #[error("invalid snapshot path {value:?}: {source}")]
    InvalidPath {
        /// Rejected path text.
        value: String,
        /// Violated path rule.
        #[source]
        source: SnapshotPathError,
    },
    /// Two distinct paths would alias on a case-insensitive filesystem.
    #[error("snapshot paths {first} and {second} collide when case-folded")]
    CaseFoldCollision {
        /// First canonical path.
        first: SnapshotPath,
        /// Conflicting canonical path.
        second: SnapshotPath,
    },
    /// A manifest omitted a directory required by a descendant.
    #[error("snapshot path {path} is missing parent directory {parent}")]
    MissingParent {
        /// Descendant path.
        path: SnapshotPath,
        /// Missing parent path.
        parent: String,
    },
    /// A descendant's parent is not a directory.
    #[error("snapshot path {path} has non-directory parent {parent}")]
    ParentNotDirectory {
        /// Descendant path.
        path: SnapshotPath,
        /// Conflicting parent path.
        parent: SnapshotPath,
    },
    /// The source contains a filesystem kind Rewind does not snapshot.
    #[error(
        "unsupported filesystem entry at {0}; only files, directories, and symlinks are supported"
    )]
    UnsupportedEntry(PathBuf),
    /// The manifest's stable JSON representation could not be encoded.
    #[error("cannot encode canonical snapshot manifest: {0}")]
    CanonicalEncoding(#[source] serde_json::Error),
    /// A supplied snapshot ID does not identify its canonical manifest.
    #[error("snapshot identity mismatch: supplied {expected}, canonical manifest is {actual}")]
    SnapshotIdentityMismatch {
        /// Supplied identity.
        expected: SnapshotId,
        /// Recomputed identity.
        actual: SnapshotId,
    },
    /// The domain manifest rejected an invariant.
    #[error("invalid snapshot manifest: {0}")]
    Manifest(#[from] SnapshotManifestError),
    /// A low-level no-follow file read or clone operation failed.
    #[error("platform filesystem operation failed: {0}")]
    Platform(#[from] FileSystemError),
    /// Immutable object storage failed.
    #[error("snapshot object operation failed: {0}")]
    Store(#[from] StoreError),
    /// A filesystem operation failed with path context.
    #[error("cannot {operation} at {path}: {source}")]
    Io {
        /// Attempted operation.
        operation: &'static str,
        /// Relevant path.
        path: PathBuf,
        /// Operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A referenced object length differs from the manifest.
    #[error("snapshot object for {path} has {actual} bytes; manifest claims {expected}")]
    ObjectSizeMismatch {
        /// Referencing path.
        path: SnapshotPath,
        /// Manifest size.
        expected: u64,
        /// Verified object size.
        actual: u64,
    },
    /// A manifest exceeds the configured restore allocation boundary.
    #[error("snapshot file {path} is {actual} bytes; configured restore maximum is {maximum}")]
    RestoreFileTooLarge {
        /// Referencing path.
        path: SnapshotPath,
        /// Manifest size.
        actual: u64,
        /// Restore limit.
        maximum: u64,
    },
    /// The destination is occupied and destructive replacement was not requested.
    #[error("checkout destination is not empty: {0}; pass force to replace it")]
    DestinationNotEmpty(PathBuf),
    /// The destination has no usable sibling directory for atomic construction.
    #[error("checkout destination has no parent or file name: {0}")]
    InvalidDestination(PathBuf),
    /// A completed tree could not replace the destination and rollback also failed.
    #[error(
        "cannot install checkout at {destination}: {install}; rollback also failed: {rollback}"
    )]
    InstallRollback {
        /// Requested checkout destination.
        destination: PathBuf,
        /// Installation failure.
        install: Box<FileSystemError>,
        /// Rollback failure.
        rollback: Box<FileSystemError>,
    },
    /// Bounded unique sibling allocation was exhausted.
    #[error("cannot allocate a private temporary sibling for {0}")]
    TemporaryNameExhausted(PathBuf),
}

/// Result type for snapshot operations.
pub type Result<T> = std::result::Result<T, SnapshotError>;

/// Applies the same strict path rules used by scans and restore preflight.
pub fn validate_relative_path(value: &str) -> Result<SnapshotPath> {
    value.parse().map_err(|source| SnapshotError::InvalidPath {
        value: value.to_owned(),
        source,
    })
}

/// Validates cross-entry relationships that cannot be expressed by one entry.
pub fn validate_manifest(manifest: &SnapshotManifest) -> Result<()> {
    let mut folded = BTreeMap::<String, &rewind_domain::SnapshotEntry>::new();
    let entries = manifest
        .entries()
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    for entry in manifest.entries() {
        let key = case_fold(entry.path.as_str());
        if let Some(previous) = folded.insert(key, entry) {
            return Err(SnapshotError::CaseFoldCollision {
                first: previous.path.clone(),
                second: entry.path.clone(),
            });
        }
    }

    for entry in manifest.entries() {
        for end in entry
            .path
            .as_str()
            .match_indices('/')
            .map(|(index, _)| index)
        {
            let parent_key = &entry.path.as_str()[..end];
            let Some(parent) = entries.get(parent_key) else {
                return Err(SnapshotError::MissingParent {
                    path: entry.path.clone(),
                    parent: parent_key.to_owned(),
                });
            };
            if !matches!(parent.kind, rewind_domain::SnapshotEntryKind::Directory) {
                return Err(SnapshotError::ParentNotDirectory {
                    path: entry.path.clone(),
                    parent: parent.path.clone(),
                });
            }
        }
    }
    Ok(())
}

fn case_fold(path: &str) -> String {
    path.chars().flat_map(char::to_lowercase).collect()
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> SnapshotError {
    SnapshotError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}
