use std::fs;
use std::path::Path;
use std::str::FromStr;

use rewind_domain::{
    Snapshot, SnapshotEntry, SnapshotEntryKind, SnapshotId, SnapshotManifest, SnapshotPath,
    UnixPermissions,
};
use rewind_platform::{
    DirectoryEntry, DirectoryEntryKind, DirectoryRoot, FileSystemError, PinnedDirectory,
};
use rewind_store::Store;

use crate::{Result, SnapshotError, io_error, validate_manifest, validate_relative_path};

/// Default maximum number of bytes retained from one workspace file (64 MiB).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// One anchored, slash-separated ignore glob.
///
/// `*` matches within one path component and `**` may cross `/`. An exact
/// directory match excludes its complete subtree because the scanner does not
/// descend into it. Patterns are always relative to the workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IgnorePattern(String);

impl IgnorePattern {
    /// Borrows the validated pattern text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn matches(&self, path: &SnapshotPath) -> bool {
        glob_matches(self.0.as_bytes(), path.as_str().as_bytes())
    }
}

impl FromStr for IgnorePattern {
    type Err = SnapshotError;

    fn from_str(value: &str) -> Result<Self> {
        validate_relative_path(value)?;
        Ok(Self(value.to_owned()))
    }
}

/// Bounded, explicit policy for an authoritative workspace scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    /// User-selected anchored ignore patterns. No path is ignored implicitly.
    pub ignore: Vec<IgnorePattern>,
    /// Maximum bytes retained from any one regular file.
    pub max_file_size: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// A path deliberately omitted from a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exclusion {
    /// Canonical path that was omitted. A directory denotes its whole subtree.
    pub path: SnapshotPath,
    /// Policy decision that caused the omission.
    pub reason: ExclusionReason,
}

/// Why a workspace path was deliberately omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExclusionReason {
    /// The path matched this explicit ignore pattern.
    Ignored {
        /// Pattern supplied by configuration.
        pattern: String,
    },
    /// A regular file exceeded the configured retention boundary.
    FileTooLarge {
        /// Observed byte length.
        actual: u64,
        /// Configured byte limit.
        maximum: u64,
    },
}

/// Canonical scan output and every deliberate omission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanReport {
    /// Deterministically identified supported workspace state.
    pub snapshot: Snapshot,
    /// Sorted visible exclusions. Excluded content is never stored.
    pub exclusions: Vec<Exclusion>,
}

/// Scans `root` without following symlinks and durably stores regular-file bytes.
///
/// Watcher hints are intentionally absent from this API: every call performs an
/// authoritative tree read. A concurrent mutation is reported rather than
/// silently combining directory states.
pub fn scan_workspace(
    root: impl AsRef<Path>,
    store: &mut Store,
    options: &ScanOptions,
    created_unix_ms: i64,
) -> Result<ScanReport> {
    let root = root.as_ref();
    let metadata = fs::symlink_metadata(root)
        .map_err(|source| io_error("inspect workspace root", root, source))?;
    if !metadata.file_type().is_dir() {
        return Err(SnapshotError::RootNotDirectory(root.to_path_buf()));
    }
    let root_directory = DirectoryRoot::open(root)?;
    let pinned_root = root_directory.pinned_directory()?;

    let mut scanner = Scanner {
        root,
        store,
        options,
        created_unix_ms,
        entries: Vec::new(),
        exclusions: Vec::new(),
    };
    scanner.scan_directory(&pinned_root, "")?;
    let manifest = SnapshotManifest::new(scanner.entries)?;
    validate_manifest(&manifest)?;
    let canonical = serde_json::to_vec(&manifest).map_err(SnapshotError::CanonicalEncoding)?;
    let snapshot = Snapshot {
        id: SnapshotId::digest(&canonical),
        manifest,
    };
    scanner
        .exclusions
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ScanReport {
        snapshot,
        exclusions: scanner.exclusions,
    })
}

struct Scanner<'a> {
    root: &'a Path,
    store: &'a mut Store,
    options: &'a ScanOptions,
    created_unix_ms: i64,
    entries: Vec<SnapshotEntry>,
    exclusions: Vec<Exclusion>,
}

impl Scanner<'_> {
    fn scan_directory(&mut self, directory: &PinnedDirectory, relative: &str) -> Result<()> {
        for child in directory.entries()? {
            let physical = self.root.join(relative).join(child.name());
            let name = child
                .name()
                .to_str()
                .ok_or_else(|| SnapshotError::NonUtf8Path(physical.clone()))?
                .to_owned();
            let relative = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let path = validate_relative_path(&relative)?;
            if let Some(pattern) = self
                .options
                .ignore
                .iter()
                .find(|pattern| pattern.matches(&path))
            {
                self.exclusions.push(Exclusion {
                    path,
                    reason: ExclusionReason::Ignored {
                        pattern: pattern.as_str().to_owned(),
                    },
                });
                continue;
            }
            self.scan_entry(directory, &child, path)?;
        }
        directory.verify_unchanged()?;
        Ok(())
    }

    fn scan_entry(
        &mut self,
        directory: &PinnedDirectory,
        entry: &DirectoryEntry,
        path: SnapshotPath,
    ) -> Result<()> {
        match entry.kind() {
            DirectoryEntryKind::Directory => {
                let child = directory.open_directory(entry)?;
                self.entries.push(SnapshotEntry {
                    path: path.clone(),
                    kind: SnapshotEntryKind::Directory,
                    permissions: permissions(child.mode()),
                });
                self.scan_directory(&child, path.as_str())?;
                directory.verify_entry(entry)?;
                Ok(())
            }
            DirectoryEntryKind::File => self.scan_file(directory, entry, path),
            DirectoryEntryKind::Symlink => self.scan_symlink(directory, entry, path),
            DirectoryEntryKind::Other => Err(SnapshotError::UnsupportedEntry(
                self.root.join(path.as_str()),
            )),
        }
    }

    fn scan_file(
        &mut self,
        directory: &PinnedDirectory,
        entry: &DirectoryEntry,
        path: SnapshotPath,
    ) -> Result<()> {
        let read = match directory.read_regular_file(entry, self.options.max_file_size) {
            Ok(read) => read,
            Err(FileSystemError::FileTooLarge {
                actual, maximum, ..
            }) => {
                self.exclusions.push(Exclusion {
                    path,
                    reason: ExclusionReason::FileTooLarge { actual, maximum },
                });
                return Ok(());
            }
            Err(source) => return Err(source.into()),
        };
        let stored = self.store.put_object(&read.bytes, self.created_unix_ms)?;
        self.entries.push(SnapshotEntry {
            path,
            kind: SnapshotEntryKind::File {
                object_id: stored.id,
                size: stored.logical_size,
                executable: read.mode & 0o111 != 0,
            },
            permissions: permissions(read.mode),
        });
        Ok(())
    }

    fn scan_symlink(
        &mut self,
        directory: &PinnedDirectory,
        entry: &DirectoryEntry,
        path: SnapshotPath,
    ) -> Result<()> {
        let target = directory.read_symlink(entry)?;
        let target = target
            .to_str()
            .ok_or_else(|| SnapshotError::NonUtf8Path(target.clone()))?
            .to_owned();
        self.entries.push(SnapshotEntry {
            path,
            kind: SnapshotEntryKind::Symlink { target },
            permissions: permissions(entry.mode()),
        });
        Ok(())
    }
}

fn permissions(mode: u32) -> UnixPermissions {
    let supported = u16::try_from(mode & 0o7777)
        .expect("a mode masked to 12 permission bits always fits in u16");
    UnixPermissions::new(supported)
        .expect("a mode masked with UnixPermissions::MASK is always valid")
}

fn glob_matches(pattern: &[u8], path: &[u8]) -> bool {
    let mut previous = vec![false; path.len() + 1];
    previous[0] = true;
    let mut index = 0;
    while index < pattern.len() {
        let recursive =
            pattern[index] == b'*' && pattern.get(index + 1).is_some_and(|next| *next == b'*');
        let wildcard = pattern[index] == b'*';
        let mut current = vec![false; path.len() + 1];
        if wildcard {
            current[0] = previous[0];
            for offset in 1..=path.len() {
                current[offset] = previous[offset]
                    || (current[offset - 1] && (recursive || path[offset - 1] != b'/'));
            }
            index += if recursive { 2 } else { 1 };
        } else {
            for offset in 1..=path.len() {
                current[offset] = previous[offset - 1] && pattern[index] == path[offset - 1];
            }
            index += 1;
        }
        previous = current;
    }
    previous[path.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_globs_distinguish_component_and_recursive_wildcards() {
        let matches = |pattern: &str, path: &str| glob_matches(pattern.as_bytes(), path.as_bytes());
        assert!(matches("*.log", "root.log"));
        assert!(!matches("*.log", "nested/root.log"));
        assert!(matches("**/*.log", "nested/root.log"));
        assert!(matches("build/**", "build/nested/output"));
        assert!(!matches("build/*", "build/nested/output"));
    }
}
