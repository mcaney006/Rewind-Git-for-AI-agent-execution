//! Narrow operating-system primitives used by Rewind.
//!
//! Product policy belongs in higher layers. This crate contains the only FFI
//! required by the workspace and converts it immediately into owned safe types.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Rewind currently supports macOS and Linux only");

mod filesystem;
mod locking;
mod paths;
mod process;
mod pty;
mod terminal;

pub use filesystem::{
    CloneReport, CloneStrategy, DirectoryChildState, DirectoryEntry, DirectoryEntryKind,
    DirectoryEntryMetadata, DirectoryRoot, DiskSpace, FileSystemError, PinnedDirectory, ReadFile,
    clone_workspace, create_private_dir, disk_space, read_regular_file_beneath,
    remove_relative_entry,
};
pub use locking::ExclusiveFileLock;
pub use paths::{ApplicationPaths, PathConventionError, application_paths};
pub use process::{ProcessInfo, ProcessObservationError, supervised_processes};
pub use pty::{
    ChildExit, PtyChild, PtyEchoProbe, PtyError, PtyMaster, PtyProcess, PtySize, spawn_pty,
};
pub use terminal::{TerminalError, TerminalModeGuard, terminal_echo_enabled, terminal_size};

use serde::{Deserialize, Serialize};

/// Capabilities selected for the current build target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Operating-system family.
    pub operating_system: &'static str,
    /// CPU architecture.
    pub architecture: &'static str,
    /// Preferred copy-on-write primitive.
    pub clone_primitive: &'static str,
    /// Filesystem notification primitive available to a future hint watcher.
    pub watcher_primitive: &'static str,
    /// Process observation source.
    pub process_source: &'static str,
    /// Whether a native pseudoterminal implementation is compiled in.
    pub pty: bool,
}

/// Returns compile-time platform capability information.
#[must_use]
pub const fn capabilities() -> PlatformCapabilities {
    #[cfg(target_os = "macos")]
    {
        PlatformCapabilities {
            operating_system: "macos",
            architecture: std::env::consts::ARCH,
            clone_primitive: "fclonefileat",
            watcher_primitive: "kqueue/fsevents",
            process_source: "libproc",
            pty: true,
        }
    }

    #[cfg(target_os = "linux")]
    {
        PlatformCapabilities {
            operating_system: "linux",
            architecture: std::env::consts::ARCH,
            clone_primitive: "FICLONE",
            watcher_primitive: "inotify",
            process_source: "/proc",
            pty: true,
        }
    }
}
