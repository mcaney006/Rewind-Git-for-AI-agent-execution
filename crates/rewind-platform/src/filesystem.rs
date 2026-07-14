use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Filesystem cloning strategy actually used for a workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneStrategy {
    /// Every regular file used APFS `fclonefileat`.
    ApfsClone,
    /// Every regular file used Linux `FICLONE`.
    LinuxReflink,
    /// Some files cloned and others required a byte copy.
    Mixed,
    /// Every regular file required a byte copy.
    RecursiveCopy,
}

/// Result of cloning a workspace tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CloneReport {
    /// Effective strategy across all regular files.
    pub strategy: CloneStrategy,
    /// Number of regular files cloned through copy-on-write.
    pub cloned_files: u64,
    /// Number of regular files copied byte-for-byte.
    pub copied_files: u64,
    /// Number of directories created.
    pub directories: u64,
    /// Number of symlinks recreated without following them.
    pub symlinks: u64,
}

/// Available and total bytes for the filesystem containing a path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiskSpace {
    /// Bytes available to an unprivileged process.
    pub available: u64,
    /// Total filesystem bytes.
    pub total: u64,
}

/// Bytes and supported metadata read through a no-follow descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadFile {
    /// Exact file bytes from one stable descriptor.
    pub bytes: Vec<u8>,
    /// Supported Unix permission and special bits.
    pub mode: u32,
}

/// Kind of one entry observed without following it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryEntryKind {
    /// A directory that can be opened relative to its pinned parent.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// A filesystem kind Rewind does not snapshot.
    Other,
}

/// State of one direct child beneath a pinned directory descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryChildState {
    /// No child currently has the requested name.
    Absent,
    /// The child is an empty directory.
    EmptyDirectory,
    /// The child is a non-directory or a nonempty directory.
    Occupied,
}

/// Supported metadata captured while classifying one directory entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryEntryMetadata {
    /// Entry kind observed without following links.
    pub kind: DirectoryEntryKind,
    /// Complete Unix mode bits reported by the filesystem.
    pub mode: u32,
    /// Logical entry size reported by stat.
    pub size: u64,
    /// Modification timestamp seconds.
    pub modified_seconds: i64,
    /// Modification timestamp nanoseconds.
    pub modified_nanoseconds: i64,
    /// Metadata-change timestamp seconds.
    pub changed_seconds: i64,
    /// Metadata-change timestamp nanoseconds.
    pub changed_nanoseconds: i64,
}

/// One entry classified relative to a pinned directory descriptor.
pub struct DirectoryEntry {
    name: CString,
    stat: EntryStat,
}

impl DirectoryEntry {
    /// Returns the single path component exactly as stored by the filesystem.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        OsStr::from_bytes(self.name.as_bytes())
    }

    /// Returns the entry kind observed without following a symlink.
    #[must_use]
    pub fn kind(&self) -> DirectoryEntryKind {
        self.stat.kind().into()
    }

    /// Returns supported Unix mode bits observed for the entry.
    #[must_use]
    pub fn mode(&self) -> u32 {
        self.stat.mode()
    }

    /// Returns the supported stat fields captured with this classification.
    #[must_use]
    pub fn metadata(&self) -> DirectoryEntryMetadata {
        DirectoryEntryMetadata {
            kind: self.kind(),
            mode: self.stat.mode(),
            size: self.stat.size(),
            modified_seconds: self.stat.modified_seconds(),
            modified_nanoseconds: self.stat.modified_nanoseconds(),
            changed_seconds: self.stat.changed_seconds(),
            changed_nanoseconds: self.stat.changed_nanoseconds(),
        }
    }
}

/// A directory pinned by an open descriptor for race-safe tree traversal.
pub struct PinnedDirectory {
    directory: File,
    path: PathBuf,
    before: fs::Metadata,
}

impl PinnedDirectory {
    /// Enumerates and classifies immediate children without following any link.
    ///
    /// Results are sorted by raw filename bytes so persisted tree construction
    /// does not depend on filesystem enumeration order.
    pub fn entries(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        self.entries_bounded(usize::MAX)
    }

    /// Enumerates at most `maximum` immediate children without following links.
    pub fn entries_bounded(&self, maximum: usize) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        self.verify_unchanged()?;
        let enumeration = open_directory_at(self.directory.as_raw_fd(), c".")
            .map_err(|error| io_error("duplicate directory for enumeration", &self.path, error))?;
        let mut stream = DirectoryStream::open(enumeration.as_raw_fd())
            .map_err(|error| io_error("open directory stream", &self.path, error))?;
        let mut entries = Vec::new();
        while let Some(name) = stream
            .next_name()
            .map_err(|error| io_error("read directory entry", &self.path, error))?
        {
            if matches!(name.as_bytes(), b"." | b"..") {
                continue;
            }
            if entries.len() == maximum {
                return Err(FileSystemError::DirectoryEntryLimit {
                    path: self.path.clone(),
                    maximum,
                });
            }
            let path = self.path.join(OsStr::from_bytes(name.as_bytes()));
            let stat = fstatat_entry(self.directory.as_raw_fd(), &name)
                .map_err(|error| changed_or_io("inspect directory entry", &path, error))?;
            entries.push(DirectoryEntry { name, stat });
        }
        entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        self.verify_unchanged()?;
        Ok(entries)
    }

    /// Opens a child directory from this descriptor and verifies its identity.
    pub fn open_directory(
        &self,
        entry: &DirectoryEntry,
    ) -> Result<PinnedDirectory, FileSystemError> {
        let path = self.path.join(entry.name());
        if entry.stat.kind() != EntryKind::Directory {
            return Err(FileSystemError::ChangedDuringRead(path));
        }
        let directory = open_directory_at(self.directory.as_raw_fd(), &entry.name)
            .map_err(|error| changed_or_io("open directory entry", &path, error))?;
        let before = directory
            .metadata()
            .map_err(|error| io_error("inspect open directory", &path, error))?;
        entry.stat.verify(&before, &path)?;
        Ok(PinnedDirectory {
            directory,
            path,
            before,
        })
    }

    /// Reads one classified regular file from its pinned parent descriptor.
    pub fn read_regular_file(
        &self,
        entry: &DirectoryEntry,
        maximum: u64,
    ) -> Result<ReadFile, FileSystemError> {
        let path = self.path.join(entry.name());
        if entry.stat.kind() != EntryKind::File {
            return Err(FileSystemError::NotRegularFile(path));
        }
        let file = open_file_at(self.directory.as_raw_fd(), &entry.name)
            .map_err(|error| changed_or_io("open regular file entry", &path, error))?;
        let before = file
            .metadata()
            .map_err(|error| io_error("inspect open file", &path, error))?;
        entry.stat.verify(&before, &path)?;
        let read = read_open_regular_file(file, before, maximum, &path)?;
        verify_named_entry(
            self.directory.as_raw_fd(),
            &entry.name,
            &read.metadata,
            &path,
        )?;
        Ok(ReadFile {
            bytes: read.bytes,
            mode: read.metadata.mode(),
        })
    }

    /// Reads one classified symlink target without following the link.
    pub fn read_symlink(&self, entry: &DirectoryEntry) -> Result<PathBuf, FileSystemError> {
        let path = self.path.join(entry.name());
        if entry.stat.kind() != EntryKind::Symlink {
            return Err(FileSystemError::ChangedDuringRead(path));
        }
        read_link_at(self.directory.as_raw_fd(), &entry.name, &entry.stat, &path)
    }

    /// Verifies that a previously classified child still names the same entry.
    pub fn verify_entry(&self, entry: &DirectoryEntry) -> Result<(), FileSystemError> {
        let path = self.path.join(entry.name());
        let current = fstatat_entry(self.directory.as_raw_fd(), &entry.name)
            .map_err(|error| changed_or_io("reinspect directory entry", &path, error))?;
        if entry.stat.same_snapshot(&current) {
            Ok(())
        } else {
            Err(FileSystemError::ChangedDuringRead(path))
        }
    }

    /// Verifies supported directory metadata did not change during traversal.
    pub fn verify_unchanged(&self) -> Result<(), FileSystemError> {
        verify_open_entry(
            &self.path,
            &self.before,
            &self.directory,
            EntryKind::Directory,
        )
    }

    /// Returns supported Unix mode bits for this open directory.
    #[must_use]
    pub fn mode(&self) -> u32 {
        self.before.mode()
    }
}

/// An opened directory that pins the root for descriptor-relative operations.
pub struct DirectoryRoot {
    directory: File,
    path: PathBuf,
}

impl DirectoryRoot {
    /// Opens a directory without following a final symlink.
    pub fn open(path: &Path) -> Result<Self, FileSystemError> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| {
                io_error(
                    "open workspace root without following symlinks",
                    path,
                    error,
                )
            })?;
        Ok(Self {
            directory,
            path: path.to_path_buf(),
        })
    }

    /// Duplicates the pinned root as a directory traversal cursor.
    pub fn pinned_directory(&self) -> Result<PinnedDirectory, FileSystemError> {
        let directory = self
            .directory
            .try_clone()
            .map_err(|error| io_error("duplicate workspace root descriptor", &self.path, error))?;
        let before = directory
            .metadata()
            .map_err(|error| io_error("inspect open workspace root", &self.path, error))?;
        Ok(PinnedDirectory {
            directory,
            path: self.path.clone(),
            before,
        })
    }

    /// Inspects one direct child without following a symbolic link.
    pub fn child_state(&self, name: &OsStr) -> Result<DirectoryChildState, FileSystemError> {
        let name = single_component(name)?;
        let display = self.path.join(OsStr::from_bytes(name.as_bytes()));
        let stat = match fstatat_entry(self.directory.as_raw_fd(), &name) {
            Ok(stat) => stat,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(DirectoryChildState::Absent);
            }
            Err(error) => return Err(io_error("inspect directory child", &display, error)),
        };
        if stat.kind() != EntryKind::Directory {
            return Ok(DirectoryChildState::Occupied);
        }
        let directory = open_directory_at(self.directory.as_raw_fd(), &name)
            .map_err(|error| changed_or_io("open directory child", &display, error))?;
        let metadata = directory
            .metadata()
            .map_err(|error| io_error("inspect open directory child", &display, error))?;
        stat.verify(&metadata, &display)?;
        let mut entries = DirectoryStream::open(directory.as_raw_fd())
            .map_err(|error| io_error("enumerate directory child", &display, error))?;
        while let Some(child) = entries
            .next_name()
            .map_err(|error| io_error("read directory child entry", &display, error))?
        {
            if !matches!(child.as_bytes(), b"." | b"..") {
                return Ok(DirectoryChildState::Occupied);
            }
        }
        verify_named_entry(self.directory.as_raw_fd(), &name, &metadata, &display)?;
        Ok(DirectoryChildState::EmptyDirectory)
    }

    /// Atomically creates and pins one private direct child directory.
    pub fn create_child_directory(&self, name: &OsStr) -> Result<DirectoryRoot, FileSystemError> {
        let name = single_component(name)?;
        let display = self.path.join(OsStr::from_bytes(name.as_bytes()));
        mkdir_at(self.directory.as_raw_fd(), &name, 0o700)
            .map_err(|error| io_error("create directory child", &display, error))?;
        let directory = match open_directory_at(self.directory.as_raw_fd(), &name) {
            Ok(directory) => directory,
            Err(error) => {
                let _ = unlink_at(self.directory.as_raw_fd(), &name, libc::AT_REMOVEDIR);
                return Err(io_error("pin created directory child", &display, error));
            }
        };
        Ok(DirectoryRoot {
            directory,
            path: display,
        })
    }

    /// Creates a private directory at a normalized relative path.
    pub fn create_directory_beneath(&self, relative: &str) -> Result<(), FileSystemError> {
        let (directory, name, display) = self.open_parent_beneath(relative)?;
        mkdir_at(directory.as_raw_fd(), &name, 0o700)
            .map_err(|error| io_error("create directory beneath root", &display, error))
    }

    /// Creates, writes, flushes, and chmods one regular file beneath this root.
    pub fn create_file_beneath(
        &self,
        relative: &str,
        bytes: &[u8],
        mode: u32,
    ) -> Result<(), FileSystemError> {
        let (directory, name, display) = self.open_parent_beneath(relative)?;
        let mut file = create_file_at(directory.as_raw_fd(), &name, 0o600)
            .map_err(|error| io_error("create file beneath root", &display, error))?;
        file.write_all(bytes)
            .map_err(|error| io_error("write file beneath root", &display, error))?;
        file.sync_all()
            .map_err(|error| io_error("flush file beneath root", &display, error))?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| io_error("set file mode beneath root", &display, error))
    }

    /// Creates one symbolic link beneath this root without traversing its target.
    pub fn create_symlink_beneath(
        &self,
        relative: &str,
        target: &OsStr,
    ) -> Result<(), FileSystemError> {
        let (directory, name, display) = self.open_parent_beneath(relative)?;
        let target = CString::new(target.as_bytes())
            .map_err(|_| FileSystemError::InteriorNul(PathBuf::from(target)))?;
        // SAFETY: both strings are NUL-terminated, `directory` is a live
        // descriptor, and symlinkat stores target bytes without traversing them.
        if unsafe { libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), name.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(io_error(
                "create symbolic link beneath root",
                &display,
                io::Error::last_os_error(),
            ))
        }
    }

    /// Applies a directory mode and flushes through the already-open descriptor.
    pub fn finish_directory_beneath(
        &self,
        relative: &str,
        mode: u32,
    ) -> Result<(), FileSystemError> {
        let components = relative_components(relative)?;
        let display = self.path.join(relative);
        let mut directory = self
            .directory
            .try_clone()
            .map_err(|error| io_error("duplicate materialization root", &self.path, error))?;
        for component in components {
            directory = open_directory_at(directory.as_raw_fd(), &component)
                .map_err(|error| changed_or_io("open materialized directory", &display, error))?;
        }
        directory
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| io_error("set materialized directory mode", &display, error))?;
        directory
            .sync_all()
            .map_err(|error| io_error("flush materialized directory", &display, error))
    }

    /// Renames one direct child to another name within this pinned directory.
    pub fn rename_child(&self, from: &OsStr, to: &OsStr) -> Result<(), FileSystemError> {
        let from = single_component(from)?;
        let to = single_component(to)?;
        let display = self.path.join(OsStr::from_bytes(to.as_bytes()));
        // SAFETY: the descriptor is live, both names are NUL-terminated direct
        // children, and renameat borrows them only for this call.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.directory.as_raw_fd(),
                to.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io_error(
                "rename directory child",
                &display,
                io::Error::last_os_error(),
            ))
        }
    }

    /// Removes one direct child recursively without following symbolic links.
    pub fn remove_child(&self, name: &OsStr) -> Result<(), FileSystemError> {
        let name = single_component(name)?;
        let display = self.path.join(OsStr::from_bytes(name.as_bytes()));
        remove_entry_at(self.directory.as_raw_fd(), &name, &display, 0)
    }

    /// Flushes directory namespace changes through the pinned descriptor.
    pub fn sync(&self) -> Result<(), FileSystemError> {
        self.directory
            .sync_all()
            .map_err(|error| io_error("flush directory", &self.path, error))
    }

    fn open_parent_beneath(
        &self,
        relative: &str,
    ) -> Result<(File, CString, PathBuf), FileSystemError> {
        let components = relative_components(relative)?;
        let display = self.path.join(relative);
        let mut directory = self
            .directory
            .try_clone()
            .map_err(|error| io_error("duplicate materialization root", &self.path, error))?;
        for component in &components[..components.len() - 1] {
            directory = open_directory_at(directory.as_raw_fd(), component)
                .map_err(|error| changed_or_io("open materialization ancestor", &display, error))?;
        }
        let name = components
            .last()
            .expect("validated relative paths always contain a component")
            .to_owned();
        Ok((directory, name, display))
    }
}

/// Errors returned by safe filesystem platform wrappers.
#[derive(Debug, Error)]
pub enum FileSystemError {
    /// The source is not a directory.
    #[error("workspace source is not a directory: {0}")]
    SourceNotDirectory(PathBuf),
    /// The destination already exists.
    #[error("workspace destination already exists: {0}")]
    DestinationExists(PathBuf),
    /// The destination would recursively contain the source.
    #[error("workspace destination is inside the source: {0}")]
    RecursiveDestination(PathBuf),
    /// A linked Git worktree could expose shared mutable metadata.
    #[error("linked or symlinked Git metadata is not yet safe to isolate: {0} is not a directory")]
    LinkedGitWorktree(PathBuf),
    /// A path cannot be passed to a native API because it contains NUL.
    #[error("path contains a NUL byte: {0}")]
    InteriorNul(PathBuf),
    /// A descriptor-relative operation was given a path outside its root.
    #[error("path must be a normalized nonempty relative path: {0}")]
    InvalidRelativePath(PathBuf),
    /// A no-follow read resolved to something other than a regular file.
    #[error("workspace entry is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    /// A file exceeded the configured allocation boundary.
    #[error("workspace file {path} is {actual} bytes; configured maximum is {maximum}")]
    FileTooLarge {
        /// File being read.
        path: PathBuf,
        /// Observed file length.
        actual: u64,
        /// Configured maximum.
        maximum: u64,
    },
    /// Memory for a bounded file read could not be reserved.
    #[error("cannot reserve {requested} bytes to read workspace file {path}")]
    AllocationFailed {
        /// File being read.
        path: PathBuf,
        /// Requested allocation size.
        requested: u64,
    },
    /// A symlink target exceeded the defensive clone boundary.
    #[error("workspace symlink target at {path} exceeds {maximum} bytes")]
    SymlinkTargetTooLarge {
        /// Symlink being cloned.
        path: PathBuf,
        /// Supported target length ceiling.
        maximum: usize,
    },
    /// An entry changed while its supported state was being read.
    #[error("workspace entry changed while being read: {0}")]
    ChangedDuringRead(PathBuf),
    /// Recursive removal reached its defensive directory-depth limit.
    #[error("workspace entry exceeds the recursive removal depth limit: {0}")]
    RemovalDepthExceeded(PathBuf),
    /// A bounded directory enumeration observed more immediate children.
    #[error("directory {path} exceeds the configured {maximum}-entry scan limit")]
    DirectoryEntryLimit {
        /// Directory being enumerated.
        path: PathBuf,
        /// Maximum immediate children accepted by the caller.
        maximum: usize,
    },
    /// A filesystem operation failed with path context.
    #[error("{operation} failed for {path}: {source}")]
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Relevant path.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
}

#[derive(Default)]
struct Counts {
    cloned: u64,
    copied: u64,
    directories: u64,
    symlinks: u64,
}

/// Recursively clones a workspace without following symlinks.
///
/// The destination must not exist. Directory and file Unix permission bits are
/// preserved; ACLs, flags, and extended attributes are not part of the initial
/// supported metadata set.
pub fn clone_workspace(source: &Path, destination: &Path) -> Result<CloneReport, FileSystemError> {
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|source_error| io_error("inspect", source, source_error))?;
    if !source_metadata.is_dir() {
        return Err(FileSystemError::SourceNotDirectory(source.to_path_buf()));
    }
    if destination.exists() {
        return Err(FileSystemError::DestinationExists(
            destination.to_path_buf(),
        ));
    }
    let source_root = DirectoryRoot::open(source)?;
    let source_metadata = source_root
        .directory
        .metadata()
        .map_err(|error| io_error("inspect open workspace root", source, error))?;
    let canonical_source =
        fs::canonicalize(source).map_err(|error| io_error("canonicalize", source, error))?;
    let canonical_metadata = fs::metadata(&canonical_source)
        .map_err(|error| io_error("inspect canonical workspace root", &canonical_source, error))?;
    if !same_entry(&source_metadata, &canonical_metadata) {
        return Err(FileSystemError::ChangedDuringRead(source.to_path_buf()));
    }
    let destination_parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = fs::canonicalize(destination_parent)
        .map_err(|error| io_error("canonicalize", destination_parent, error))?;
    let destination_name = destination
        .file_name()
        .ok_or_else(|| FileSystemError::RecursiveDestination(destination.to_path_buf()))?;
    let canonical_destination = canonical_parent.join(destination_name);
    if canonical_destination.starts_with(&canonical_source) {
        return Err(FileSystemError::RecursiveDestination(
            destination.to_path_buf(),
        ));
    }

    match fstatat_entry(source_root.directory.as_raw_fd(), c".git") {
        Ok(entry) if entry.kind() != EntryKind::Directory => {
            return Err(FileSystemError::LinkedGitWorktree(
                canonical_source.join(".git"),
            ));
        }
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
        Err(error) => {
            return Err(io_error(
                "inspect Git metadata without following symlinks",
                &canonical_source.join(".git"),
                error,
            ));
        }
    }
    let mut counts = Counts::default();
    let copy = fs::create_dir(&canonical_destination)
        .map_err(|error| io_error("create directory", &canonical_destination, error))
        .and_then(|()| {
            counts.directories += 1;
            clone_directory_contents(
                &source_root.directory,
                &canonical_source,
                &canonical_destination,
                &mut counts,
            )?;
            let cloned_git = canonical_destination.join(".git");
            match fs::symlink_metadata(&cloned_git) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(FileSystemError::LinkedGitWorktree(
                        canonical_source.join(".git"),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error("inspect cloned Git metadata", &cloned_git, error));
                }
            }
            verify_open_entry(
                &canonical_source,
                &source_metadata,
                &source_root.directory,
                EntryKind::Directory,
            )?;
            fs::set_permissions(
                &canonical_destination,
                fs::Permissions::from_mode(source_metadata.permissions().mode()),
            )
            .map_err(|error| io_error("set permissions", &canonical_destination, error))
        });
    if let Err(error) = copy {
        let _ = fs::remove_dir_all(&canonical_destination);
        return Err(error);
    }

    let strategy = match (counts.cloned, counts.copied) {
        (0, _) => CloneStrategy::RecursiveCopy,
        (_, 0) => native_clone_strategy(),
        _ => CloneStrategy::Mixed,
    };
    Ok(CloneReport {
        strategy,
        cloned_files: counts.cloned,
        copied_files: counts.copied,
        directories: counts.directories,
        symlinks: counts.symlinks,
    })
}

/// Creates a directory tree with owner-only permissions.
pub fn create_private_dir(path: &Path) -> Result<(), FileSystemError> {
    fs::create_dir_all(path).map_err(|error| io_error("create directory", path, error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io_error("set permissions", path, error))
}

/// Returns disk capacity for the filesystem containing `path`.
pub fn disk_space(path: &Path) -> Result<DiskSpace, FileSystemError> {
    let path_bytes = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| FileSystemError::InteriorNul(path.to_path_buf()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path_bytes` is NUL-terminated and `stats` points to writable,
    // correctly aligned storage for `statvfs`. The OS initializes it on success.
    let result = unsafe { libc::statvfs(path_bytes.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(io_error(
            "read disk space",
            path,
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: `statvfs` returned success and therefore initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    Ok(DiskSpace {
        available: stats.f_bavail.saturating_mul(stats.f_frsize),
        total: stats.f_blocks.saturating_mul(stats.f_frsize),
    })
}

/// Reads one bounded regular file beneath `root` without following symlinks.
///
/// Every relative component is opened from the preceding directory descriptor.
/// The final descriptor pins the inode against rename races, and metadata is
/// checked before and after reading so an in-place write becomes a visible
/// failure rather than a silently mixed object.
pub fn read_regular_file_beneath(
    root: &DirectoryRoot,
    relative: &str,
    maximum: u64,
) -> Result<ReadFile, FileSystemError> {
    let components = relative_components(relative)?;
    let display = root.path.join(relative);
    let mut directory = root
        .directory
        .try_clone()
        .map_err(|error| io_error("duplicate workspace root descriptor", &root.path, error))?;
    for component in &components[..components.len() - 1] {
        directory = open_directory_at(directory.as_raw_fd(), component).map_err(|error| {
            io_error(
                "open ancestor directory without following symlinks",
                &display,
                error,
            )
        })?;
    }
    let final_component = components
        .last()
        .expect("validated relative paths always contain a component");
    let file = open_file_at(directory.as_raw_fd(), final_component).map_err(|error| {
        io_error(
            "open file beneath workspace without following symlinks",
            &display,
            error,
        )
    })?;
    let before = file
        .metadata()
        .map_err(|error| io_error("inspect open file", &display, error))?;
    if !before.is_file() {
        return Err(FileSystemError::NotRegularFile(display));
    }
    let read = read_open_regular_file(file, before, maximum, &display)?;
    verify_named_entry(
        directory.as_raw_fd(),
        final_component,
        &read.metadata,
        &display,
    )?;
    Ok(ReadFile {
        bytes: read.bytes,
        mode: read.metadata.mode(),
    })
}

struct OpenFileRead {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
}

fn read_open_regular_file(
    mut file: File,
    before: fs::Metadata,
    maximum: u64,
    display: &Path,
) -> Result<OpenFileRead, FileSystemError> {
    if !before.is_file() {
        return Err(FileSystemError::NotRegularFile(display.to_path_buf()));
    }
    if before.len() > maximum {
        return Err(FileSystemError::FileTooLarge {
            path: display.to_path_buf(),
            actual: before.len(),
            maximum,
        });
    }
    let capacity = usize::try_from(before.len()).map_err(|_| FileSystemError::FileTooLarge {
        path: display.to_path_buf(),
        actual: before.len(),
        maximum,
    })?;
    let read_limit = maximum.saturating_add(1);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| FileSystemError::AllocationFailed {
            path: display.to_path_buf(),
            requested: before.len(),
        })?;
    let mut remaining = read_limit;
    let mut chunk = [0_u8; 64 * 1024];
    while remaining != 0 {
        let requested = usize::try_from(remaining.min(chunk.len() as u64))
            .expect("the requested chunk is bounded by its usize-sized buffer");
        let count = file
            .read(&mut chunk[..requested])
            .map_err(|error| io_error("read file", display, error))?;
        if count == 0 {
            break;
        }
        bytes
            .try_reserve_exact(count)
            .map_err(|_| FileSystemError::AllocationFailed {
                path: display.to_path_buf(),
                requested: u64::try_from(bytes.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(count).unwrap_or(u64::MAX)),
            })?;
        bytes.extend_from_slice(&chunk[..count]);
        remaining = remaining.saturating_sub(u64::try_from(count).unwrap_or(u64::MAX));
    }
    let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual > maximum {
        return Err(FileSystemError::FileTooLarge {
            path: display.to_path_buf(),
            actual,
            maximum,
        });
    }
    let after = file
        .metadata()
        .map_err(|error| io_error("inspect read file", display, error))?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || after.len() != actual
    {
        return Err(FileSystemError::ChangedDuringRead(display.to_path_buf()));
    }
    Ok(OpenFileRead {
        bytes,
        metadata: before,
    })
}

/// Removes one normalized relative entry beneath `root` without following any
/// symlink component.
///
/// Missing paths and paths hidden behind a non-directory ancestor are already
/// absent from the requested workspace location and therefore succeed.
pub fn remove_relative_entry(root: &DirectoryRoot, relative: &str) -> Result<(), FileSystemError> {
    let components = relative_components(relative)?;
    let display = root.path.join(relative);
    let mut directory = root
        .directory
        .try_clone()
        .map_err(|error| io_error("duplicate workspace root descriptor", &root.path, error))?;
    for component in &components[..components.len() - 1] {
        match open_directory_at(directory.as_raw_fd(), component) {
            Ok(next) => directory = next,
            Err(error) if path_is_absent_or_not_directory(&error) => return Ok(()),
            Err(error) => {
                return Err(io_error(
                    "open ancestor directory without following symlinks",
                    &display,
                    error,
                ));
            }
        }
    }
    let final_component = components
        .last()
        .expect("validated relative paths always contain a component");
    remove_entry_at(directory.as_raw_fd(), final_component, &display, 0)
}

const MAX_REMOVAL_DEPTH: usize = 256;

fn relative_components(relative: &str) -> Result<Vec<CString>, FileSystemError> {
    let invalid = || FileSystemError::InvalidRelativePath(PathBuf::from(relative));
    if relative.is_empty() || relative.starts_with('/') {
        return Err(invalid());
    }
    relative
        .split('/')
        .map(|component| {
            if component.is_empty() || matches!(component, "." | "..") {
                return Err(invalid());
            }
            CString::new(component.as_bytes())
                .map_err(|_| FileSystemError::InteriorNul(PathBuf::from(relative)))
        })
        .collect()
}

fn single_component(name: &OsStr) -> Result<CString, FileSystemError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || matches!(bytes, b"." | b"..") || bytes.contains(&b'/') {
        return Err(FileSystemError::InvalidRelativePath(PathBuf::from(name)));
    }
    CString::new(bytes).map_err(|_| FileSystemError::InteriorNul(PathBuf::from(name)))
}

fn mkdir_at(parent: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `parent` is a live directory descriptor, `name` is NUL-terminated,
    // and mkdirat borrows both only for this call.
    if unsafe { libc::mkdirat(parent, name.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn open_directory_at(parent: RawFd, name: &CStr) -> io::Result<File> {
    // SAFETY: `parent` is a live directory descriptor, `name` is NUL-terminated,
    // no create flag is present, and the returned descriptor is owned on success.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor that has not been wrapped.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn open_file_at(parent: RawFd, name: &CStr) -> io::Result<File> {
    // SAFETY: `parent` is a live directory descriptor, `name` is NUL-terminated,
    // no create flag is present, and O_NOFOLLOW rejects a final symlink.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor that has not been wrapped.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn create_file_at(parent: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: `parent` is a live directory descriptor, `name` is NUL-terminated,
    // O_EXCL prevents replacement, O_NOFOLLOW rejects links, and the returned
    // descriptor is exclusively owned on success.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            libc::c_uint::from(mode),
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned one new owned descriptor not wrapped elsewhere.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn remove_entry_at(
    parent: RawFd,
    name: &CStr,
    display: &Path,
    depth: usize,
) -> Result<(), FileSystemError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            if depth >= MAX_REMOVAL_DEPTH {
                return Err(FileSystemError::RemovalDepthExceeded(display.to_path_buf()));
            }
            remove_directory_contents(&directory, display, depth)?;
            verify_named_directory(parent, name, &directory, display)?;
            unlink_at(parent, name, libc::AT_REMOVEDIR)
                .map_err(|error| io_error("remove directory", display, error))
        }
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        Err(error) if path_is_not_directory(&error) => unlink_at(parent, name, 0)
            .or_else(|unlink_error| {
                if unlink_error.raw_os_error() == Some(libc::ENOENT) {
                    Ok(())
                } else {
                    Err(unlink_error)
                }
            })
            .map_err(|unlink_error| io_error("remove file or symlink", display, unlink_error)),
        Err(error) => Err(io_error(
            "open removal target without following symlinks",
            display,
            error,
        )),
    }
}

fn remove_directory_contents(
    directory: &File,
    display: &Path,
    depth: usize,
) -> Result<(), FileSystemError> {
    let mut entries = DirectoryStream::open(directory.as_raw_fd())
        .map_err(|error| io_error("open directory stream", display, error))?;
    while let Some(name) = entries
        .next_name()
        .map_err(|error| io_error("read directory entry", display, error))?
    {
        if matches!(name.as_bytes(), b"." | b"..") {
            continue;
        }
        let child = display.join(OsStr::from_bytes(name.as_bytes()));
        remove_entry_at(directory.as_raw_fd(), &name, &child, depth + 1)?;
    }
    Ok(())
}

fn verify_named_directory(
    parent: RawFd,
    name: &CStr,
    pinned: &File,
    display: &Path,
) -> Result<(), FileSystemError> {
    let pinned = pinned
        .metadata()
        .map_err(|error| io_error("inspect open removal directory", display, error))?;
    let named = open_directory_at(parent, name)
        .and_then(|directory| directory.metadata())
        .map_err(|_| FileSystemError::ChangedDuringRead(display.to_path_buf()))?;
    if pinned.dev() == named.dev() && pinned.ino() == named.ino() && named.is_dir() {
        Ok(())
    } else {
        Err(FileSystemError::ChangedDuringRead(display.to_path_buf()))
    }
}

fn unlink_at(parent: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: `parent` is a live directory descriptor and `name` is a valid
    // NUL-terminated entry name. `unlinkat` never follows the removed entry.
    if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn path_is_absent_or_not_directory(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOENT) || path_is_not_directory(error)
}

fn path_is_not_directory(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
}

struct DirectoryStream(*mut libc::DIR);

impl DirectoryStream {
    fn open(directory: RawFd) -> io::Result<Self> {
        // SAFETY: `directory` is live for this call. F_DUPFD_CLOEXEC creates a
        // distinct owned descriptor for `fdopendir` to consume.
        let duplicate = unsafe { libc::fcntl(directory, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `duplicate` is an owned directory descriptor. On success the
        // DIR stream owns it; on failure ownership remains here and is closed.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed and did not take ownership of `duplicate`.
            unsafe { libc::close(duplicate) };
            return Err(error);
        }
        Ok(Self(stream))
    }

    fn next_name(&mut self) -> io::Result<Option<CString>> {
        clear_errno();
        // SAFETY: `self.0` is a live DIR pointer exclusively borrowed here.
        let entry = unsafe { libc::readdir(self.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: `readdir` returned a live dirent whose d_name is NUL-terminated
        // for the duration of this call; `to_owned` copies it before the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
        Ok(Some(name))
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: `self.0` is owned by this wrapper and closed exactly once.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "macos")]
fn clear_errno() {
    // SAFETY: `__error` returns the calling thread's writable errno pointer.
    unsafe { *libc::__error() = 0 };
}

#[cfg(target_os = "linux")]
fn clear_errno() {
    // SAFETY: `__errno_location` returns the calling thread's writable errno pointer.
    unsafe { *libc::__errno_location() = 0 };
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl From<EntryKind> for DirectoryEntryKind {
    fn from(value: EntryKind) -> Self {
        match value {
            EntryKind::Directory => Self::Directory,
            EntryKind::File => Self::File,
            EntryKind::Symlink => Self::Symlink,
            EntryKind::Other => Self::Other,
        }
    }
}

struct EntryStat(libc::stat);

impl EntryStat {
    fn kind(&self) -> EntryKind {
        match self.0.st_mode & libc::S_IFMT {
            libc::S_IFDIR => EntryKind::Directory,
            libc::S_IFREG => EntryKind::File,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }

    fn mode(&self) -> u32 {
        self.0.st_mode
    }

    fn size(&self) -> u64 {
        u64::try_from(self.0.st_size).unwrap_or(u64::MAX)
    }

    fn modified_seconds(&self) -> i64 {
        self.0.st_mtime
    }

    fn modified_nanoseconds(&self) -> i64 {
        self.0.st_mtime_nsec
    }

    fn changed_seconds(&self) -> i64 {
        self.0.st_ctime
    }

    fn changed_nanoseconds(&self) -> i64 {
        self.0.st_ctime_nsec
    }

    fn verify(&self, metadata: &fs::Metadata, path: &Path) -> Result<(), FileSystemError> {
        if stat_matches_metadata(&self.0, metadata) {
            Ok(())
        } else {
            Err(FileSystemError::ChangedDuringRead(path.to_path_buf()))
        }
    }

    fn same_snapshot(&self, other: &Self) -> bool {
        stat_snapshots_match(&self.0, &other.0)
    }
}

#[cfg(target_os = "macos")]
fn stat_matches_metadata(stat: &libc::stat, metadata: &fs::Metadata) -> bool {
    u64::try_from(stat.st_dev).ok() == Some(metadata.dev())
        && stat.st_ino == metadata.ino()
        && u32::from(stat.st_mode) == metadata.mode()
        && u64::try_from(stat.st_size).ok() == Some(metadata.len())
        && stat.st_mtime == metadata.mtime()
        && stat.st_mtime_nsec == metadata.mtime_nsec()
        && stat.st_ctime == metadata.ctime()
        && stat.st_ctime_nsec == metadata.ctime_nsec()
}

#[cfg(target_os = "linux")]
fn stat_matches_metadata(stat: &libc::stat, metadata: &fs::Metadata) -> bool {
    stat.st_dev == metadata.dev()
        && stat.st_ino == metadata.ino()
        && stat.st_mode == metadata.mode()
        && u64::try_from(stat.st_size).ok() == Some(metadata.len())
        && stat.st_mtime == metadata.mtime()
        && stat.st_mtime_nsec == metadata.mtime_nsec()
        && stat.st_ctime == metadata.ctime()
        && stat.st_ctime_nsec == metadata.ctime_nsec()
}

#[cfg(target_os = "macos")]
fn stat_snapshots_match(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_mode == right.st_mode
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

#[cfg(target_os = "linux")]
fn stat_snapshots_match(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_mode == right.st_mode
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn fstatat_entry(parent: RawFd, name: &CStr) -> io::Result<EntryStat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` is a live directory descriptor, `name` is NUL-terminated,
    // and `stat` points to aligned writable storage initialized on success.
    let result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstatat` returned success and initialized the complete stat value.
    Ok(EntryStat(unsafe { stat.assume_init() }))
}

fn verify_named_entry(
    parent: RawFd,
    name: &CStr,
    expected: &fs::Metadata,
    path: &Path,
) -> Result<(), FileSystemError> {
    let current = fstatat_entry(parent, name)
        .map_err(|_| FileSystemError::ChangedDuringRead(path.to_path_buf()))?;
    current.verify(expected, path)
}

const MAX_SYMLINK_TARGET_BYTES: usize = 1024 * 1024;

fn read_link_at(
    parent: RawFd,
    name: &CStr,
    expected: &EntryStat,
    path: &Path,
) -> Result<PathBuf, FileSystemError> {
    // ponytail: 1 MiB is far above current macOS/Linux symlink limits; raise it
    // only if a supported filesystem demonstrates a larger legitimate target.
    let hinted = usize::try_from(expected.0.st_size)
        .unwrap_or(0)
        .saturating_add(1)
        .max(256);
    let mut capacity = hinted.min(MAX_SYMLINK_TARGET_BYTES);
    loop {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| FileSystemError::AllocationFailed {
                path: path.to_path_buf(),
                requested: u64::try_from(capacity).unwrap_or(u64::MAX),
            })?;
        bytes.resize(capacity, 0);
        // SAFETY: `parent` and `name` identify an entry without traversal, and
        // `bytes` exposes `capacity` writable initialized bytes for readlinkat.
        let length = unsafe {
            libc::readlinkat(
                parent,
                name.as_ptr(),
                bytes.as_mut_ptr().cast::<libc::c_char>(),
                capacity,
            )
        };
        if length < 0 {
            return Err(changed_or_io(
                "read source symlink without following it",
                path,
                io::Error::last_os_error(),
            ));
        }
        let length = usize::try_from(length)
            .map_err(|_| FileSystemError::ChangedDuringRead(path.to_path_buf()))?;
        if length < capacity {
            bytes.truncate(length);
            let after = fstatat_entry(parent, name)
                .map_err(|_| FileSystemError::ChangedDuringRead(path.to_path_buf()))?;
            if !expected.same_snapshot(&after) {
                return Err(FileSystemError::ChangedDuringRead(path.to_path_buf()));
            }
            return Ok(PathBuf::from(OsStr::from_bytes(&bytes)));
        }
        if capacity == MAX_SYMLINK_TARGET_BYTES {
            return Err(FileSystemError::SymlinkTargetTooLarge {
                path: path.to_path_buf(),
                maximum: MAX_SYMLINK_TARGET_BYTES,
            });
        }
        capacity = capacity.saturating_mul(2).min(MAX_SYMLINK_TARGET_BYTES);
    }
}

fn clone_directory_contents(
    source: &File,
    source_display: &Path,
    destination: &Path,
    counts: &mut Counts,
) -> Result<(), FileSystemError> {
    let mut entries = DirectoryStream::open(source.as_raw_fd())
        .map_err(|error| io_error("open source directory stream", source_display, error))?;
    while let Some(name) = entries
        .next_name()
        .map_err(|error| io_error("read source directory entry", source_display, error))?
    {
        if matches!(name.as_bytes(), b"." | b"..") {
            continue;
        }
        let os_name = OsStr::from_bytes(name.as_bytes());
        clone_entry(
            source.as_raw_fd(),
            &name,
            &source_display.join(os_name),
            &destination.join(os_name),
            counts,
        )?;
    }
    Ok(())
}

fn clone_entry(
    parent: RawFd,
    name: &CStr,
    source_display: &Path,
    destination: &Path,
    counts: &mut Counts,
) -> Result<(), FileSystemError> {
    let expected = fstatat_entry(parent, name).map_err(|error| {
        io_error(
            "inspect source entry without following symlinks",
            source_display,
            error,
        )
    })?;
    match expected.kind() {
        EntryKind::Directory => {
            let directory = open_directory_at(parent, name).map_err(|error| {
                io_error(
                    "open source directory without following symlinks",
                    source_display,
                    error,
                )
            })?;
            let before = directory.metadata().map_err(|error| {
                io_error("inspect open source directory", source_display, error)
            })?;
            expected.verify(&before, source_display)?;
            fs::create_dir(destination)
                .map_err(|error| io_error("create directory", destination, error))?;
            counts.directories += 1;
            clone_directory_contents(&directory, source_display, destination, counts)?;
            verify_open_entry(source_display, &before, &directory, EntryKind::Directory)?;
            verify_named_entry(parent, name, &before, source_display)?;
            fs::set_permissions(
                destination,
                fs::Permissions::from_mode(before.permissions().mode()),
            )
            .map_err(|error| io_error("set permissions", destination, error))
        }
        EntryKind::File => {
            let source = open_file_at(parent, name).map_err(|error| {
                io_error(
                    "open clone source without following symlinks",
                    source_display,
                    error,
                )
            })?;
            let before = source
                .metadata()
                .map_err(|error| io_error("inspect open clone source", source_display, error))?;
            expected.verify(&before, source_display)?;
            if native_clone_file(&source, destination, &before, source_display)? {
                counts.cloned += 1;
            } else {
                copy_open_file(&source, destination, &before, source_display)?;
                counts.copied += 1;
            }
            verify_named_entry(parent, name, &before, source_display)?;
            fs::set_permissions(
                destination,
                fs::Permissions::from_mode(before.permissions().mode()),
            )
            .map_err(|error| io_error("set permissions", destination, error))
        }
        EntryKind::Symlink => {
            let target = read_link_at(parent, name, &expected, source_display)?;
            symlink(&target, destination)
                .map_err(|error| io_error("create symlink", destination, error))?;
            counts.symlinks += 1;
            Ok(())
        }
        EntryKind::Other => Err(io_error(
            "copy unsupported filesystem entry",
            source_display,
            io::Error::new(
                io::ErrorKind::Unsupported,
                "only files, directories, and symlinks are supported",
            ),
        )),
    }
}

#[cfg(target_os = "macos")]
fn native_clone_strategy() -> CloneStrategy {
    CloneStrategy::ApfsClone
}

#[cfg(target_os = "linux")]
fn native_clone_strategy() -> CloneStrategy {
    CloneStrategy::LinuxReflink
}

#[cfg(target_os = "macos")]
fn native_clone_file(
    source: &File,
    destination: &Path,
    expected: &fs::Metadata,
    source_display: &Path,
) -> Result<bool, FileSystemError> {
    // SAFETY: This declaration matches clonefile.h on macOS; the call below
    // passes owned descriptors and a live NUL-terminated destination path.
    unsafe extern "C" {
        fn fclonefileat(
            source: libc::c_int,
            destination_directory: libc::c_int,
            destination: *const libc::c_char,
            flags: u32,
        ) -> libc::c_int;
    }
    let destination_c = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| FileSystemError::InteriorNul(destination.to_path_buf()))?;
    // SAFETY: `source` is a pinned regular-file descriptor, `destination_c` is
    // valid for the call, AT_FDCWD selects its absolute parent, and the absent
    // destination is created atomically by fclonefileat.
    let result = unsafe {
        fclonefileat(
            source.as_raw_fd(),
            libc::AT_FDCWD,
            destination_c.as_ptr(),
            0,
        )
    };
    if result == 0 {
        verify_open_entry(source_display, expected, source, EntryKind::File)?;
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    remove_partial_file(destination)?;
    if matches!(
        error.raw_os_error(),
        Some(libc::EXDEV | libc::ENOTSUP | libc::EOPNOTSUPP | libc::EINVAL)
    ) {
        return Ok(false);
    }
    Err(io_error("clone file", destination, error))
}

#[cfg(target_os = "linux")]
fn native_clone_file(
    source: &File,
    destination: &Path,
    expected: &fs::Metadata,
    source_display: &Path,
) -> Result<bool, FileSystemError> {
    const FICLONE: libc::c_ulong = 0x4004_9409;
    verify_open_entry(source_display, expected, source, EntryKind::File)?;
    let destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(destination)
        .map_err(|error| io_error("create clone destination", destination, error))?;
    // SAFETY: Both descriptors are valid regular files and FICLONE takes the
    // source descriptor by value. The destination was created exclusively.
    let result = unsafe { libc::ioctl(destination_file.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if result == 0 {
        verify_open_entry(source_display, expected, source, EntryKind::File)?;
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    drop(destination_file);
    fs::remove_file(destination)
        .map_err(|remove_error| io_error("remove failed reflink", destination, remove_error))?;
    if matches!(
        error.raw_os_error(),
        Some(libc::EXDEV | libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL)
    ) {
        return Ok(false);
    }
    Err(io_error("reflink file", destination, error))
}

fn copy_open_file(
    source: &File,
    destination: &Path,
    expected: &fs::Metadata,
    source_display: &Path,
) -> Result<(), FileSystemError> {
    let mut source_file = source
        .try_clone()
        .map_err(|error| io_error("duplicate clone source descriptor", source_display, error))?;
    verify_open_entry(source_display, expected, &source_file, EntryKind::File)?;
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(destination)
        .map_err(|error| io_error("create copy destination", destination, error))?;
    let result = io::copy(&mut source_file, &mut destination_file)
        .and_then(|_| destination_file.sync_all())
        .map_err(|error| io_error("copy file bytes", source_display, error))
        .and_then(|()| verify_open_entry(source_display, expected, &source_file, EntryKind::File));
    if result.is_err() {
        drop(destination_file);
        let _ = fs::remove_file(destination);
    }
    result
}

fn verify_open_entry(
    path: &Path,
    expected: &fs::Metadata,
    file: &File,
    kind: EntryKind,
) -> Result<(), FileSystemError> {
    let actual = file
        .metadata()
        .map_err(|error| io_error("inspect open file", path, error))?;
    let actual_kind = if actual.is_dir() {
        EntryKind::Directory
    } else if actual.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    if actual_kind == kind && same_entry(expected, &actual) {
        Ok(())
    } else {
        Err(FileSystemError::ChangedDuringRead(path.to_path_buf()))
    }
}

fn same_entry(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() == after.mode()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> FileSystemError {
    FileSystemError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn changed_or_io(operation: &'static str, path: &Path, source: io::Error) -> FileSystemError {
    if matches!(
        source.raw_os_error(),
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::ESTALE | libc::EINVAL)
    ) {
        FileSystemError::ChangedDuringRead(path.to_path_buf())
    } else {
        io_error(operation, path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn clone_preserves_supported_tree_without_linking_writable_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        let mut file = File::create(source.join("nested/tool")).unwrap();
        file.write_all(b"before").unwrap();
        fs::set_permissions(
            source.join("nested/tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("nested/tool", source.join("link")).unwrap();

        let report = clone_workspace(&source, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("nested/tool")).unwrap(),
            b"before"
        );
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            PathBuf::from("nested/tool")
        );
        assert_eq!(
            fs::metadata(destination.join("nested/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        fs::write(destination.join("nested/tool"), b"after").unwrap();
        assert_eq!(fs::read(source.join("nested/tool")).unwrap(), b"before");
        assert_eq!(report.cloned_files + report.copied_files, 1);
    }

    #[test]
    fn pinned_clone_directory_ignores_a_swapped_symlink_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let parked = temp.path().join("parked");
        let outside = temp.path().join("outside");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("ancestor")).unwrap();
        fs::write(source.join("ancestor/victim"), b"inside").unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("victim"), b"outside-sentinel").unwrap();

        let root = DirectoryRoot::open(&source).unwrap();
        let pinned = open_directory_at(root.directory.as_raw_fd(), c"ancestor").unwrap();
        fs::rename(source.join("ancestor"), &parked).unwrap();
        symlink(&outside, source.join("ancestor")).unwrap();
        fs::create_dir(&destination).unwrap();

        let mut counts = Counts::default();
        clone_directory_contents(&pinned, &source.join("ancestor"), &destination, &mut counts)
            .unwrap();

        assert_eq!(fs::read(destination.join("victim")).unwrap(), b"inside");
        assert_eq!(
            fs::read(outside.join("victim")).unwrap(),
            b"outside-sentinel"
        );
    }

    #[test]
    fn pinned_scan_entries_never_switch_to_a_replaced_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let parked = temp.path().join("parked");
        let outside = temp.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join("ancestor")).unwrap();
        fs::create_dir(workspace.join("ancestor/inside-directory")).unwrap();
        fs::set_permissions(
            workspace.join("ancestor/inside-directory"),
            fs::Permissions::from_mode(0o710),
        )
        .unwrap();
        symlink("inside-target", workspace.join("ancestor/inside-link")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(outside.join("outside-name")).unwrap();
        symlink("outside-target", outside.join("inside-link")).unwrap();

        let root = DirectoryRoot::open(&workspace).unwrap();
        let root = root.pinned_directory().unwrap();
        let ancestor = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.name() == OsStr::new("ancestor"))
            .unwrap();
        let pinned = root.open_directory(&ancestor).unwrap();
        let children = pinned.entries().unwrap();

        fs::rename(workspace.join("ancestor"), &parked).unwrap();
        symlink(&outside, workspace.join("ancestor")).unwrap();

        let names = children
            .iter()
            .map(|entry| entry.name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["inside-directory", "inside-link"]);
        let directory = children
            .iter()
            .find(|entry| entry.name() == OsStr::new("inside-directory"))
            .unwrap();
        assert_eq!(
            pinned.open_directory(directory).unwrap().mode() & 0o7777,
            0o710
        );
        let link = children
            .iter()
            .find(|entry| entry.name() == OsStr::new("inside-link"))
            .unwrap();
        assert_eq!(
            pinned.read_symlink(link).unwrap(),
            Path::new("inside-target")
        );
        assert!(matches!(
            root.verify_entry(&ancestor),
            Err(FileSystemError::ChangedDuringRead(_))
        ));
    }

    #[test]
    fn pinned_materialization_parent_ignores_namespace_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let parked = temp.path().join("parked");
        let outside = temp.path().join("outside");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&outside).unwrap();
        let root = DirectoryRoot::open(&parent).unwrap();

        fs::rename(&parent, &parked).unwrap();
        symlink(&outside, &parent).unwrap();

        let temporary = root
            .create_child_directory(OsStr::new("temporary"))
            .unwrap();
        temporary.create_directory_beneath("nested").unwrap();
        temporary
            .create_file_beneath("nested/file", b"inside", 0o600)
            .unwrap();
        temporary
            .create_symlink_beneath("link", OsStr::new("nested/file"))
            .unwrap();
        temporary.finish_directory_beneath("nested", 0o000).unwrap();
        temporary.sync().unwrap();
        root.rename_child(OsStr::new("temporary"), OsStr::new("checkout"))
            .unwrap();
        root.sync().unwrap();

        assert!(!outside.join("checkout").exists());
        assert_eq!(
            fs::symlink_metadata(parked.join("checkout/nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0
        );
        fs::set_permissions(
            parked.join("checkout/nested"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert_eq!(
            fs::read(parked.join("checkout/nested/file")).unwrap(),
            b"inside"
        );
    }

    #[test]
    fn rejects_destination_inside_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let result = clone_workspace(&source, &source.join("child"));
        assert!(matches!(
            result,
            Err(FileSystemError::RecursiveDestination(_))
        ));
    }

    #[test]
    fn beneath_reader_bounds_bytes_and_rejects_symlink_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();
        let root_handle = DirectoryRoot::open(&root).unwrap();
        let read = read_regular_file_beneath(&root_handle, "file", 7).unwrap();
        assert_eq!(read.bytes, b"content");
        assert!(matches!(
            read_regular_file_beneath(&root_handle, "file", 6),
            Err(FileSystemError::FileTooLarge { .. })
        ));
        symlink(&outside, root.join("link")).unwrap();
        assert!(read_regular_file_beneath(&root_handle, "link/sentinel", 100).is_err());
    }

    #[test]
    fn relative_removal_never_follows_symlink_ancestors_or_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let root_handle = DirectoryRoot::open(&root).unwrap();

        remove_relative_entry(&root_handle, "link/sentinel").unwrap();
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
        remove_relative_entry(&root_handle, "link").unwrap();
        assert!(fs::symlink_metadata(root.join("link")).is_err());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");

        fs::create_dir_all(root.join("tree/nested")).unwrap();
        fs::write(root.join("tree/nested/file"), b"inside").unwrap();
        remove_relative_entry(&root_handle, "tree").unwrap();
        assert!(!root.join("tree").exists());
        remove_relative_entry(&root_handle, "already/missing").unwrap();
        assert!(matches!(
            remove_relative_entry(&root_handle, "../outside/sentinel"),
            Err(FileSystemError::InvalidRelativePath(_))
        ));
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
    }

    #[test]
    fn byte_copy_never_follows_a_file_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        let destination = temp.path().join("destination");
        fs::create_dir(&root).unwrap();
        fs::write(&outside, b"sensitive").unwrap();
        symlink(&outside, root.join("link")).unwrap();

        let root = DirectoryRoot::open(&root).unwrap();
        assert!(open_file_at(root.directory.as_raw_fd(), c"link").is_err());
        assert!(!destination.exists());
    }
}
