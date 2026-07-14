//! Supervised PTY capture and bounded, single-writer recording.
//!
//! [`capture`] clones a source workspace, records one command through a real
//! pseudoterminal, commits initial/manual/final snapshots, and leaves the source
//! untouched. Producers block on a bounded channel; this crate has no lossy
//! recording mode.

#![deny(missing_docs)]

mod control;
mod recorder;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

pub use control::{
    CONTROL_PROTOCOL_VERSION, ControlClientError, ControlCommand, ControlDecodeError,
    ControlRequest, MAX_CONTROL_FRAME_BYTES, MarkOutcome, control_socket_path,
    decode_control_frame, encode_control_frame, request_marker,
};
use rewind_domain::{
    BranchId, CapturePolicy, InputRecordingPolicy, ProcessExitStatus, RunId, RunParent, RunStatus,
    SnapshotId,
};
use rewind_platform::{CloneStrategy, FileSystemError, PtyError, PtySize, TerminalError};
use rewind_snapshot::{ScanOptions, SnapshotError};
use rewind_store::StoreError;
use thiserror::Error;

/// Hard safety ceiling for one in-memory terminal frame.
pub const MAX_TERMINAL_CHUNK_BYTES: usize = 1024 * 1024;

const MAX_CHANNEL_CAPACITY: usize = 65_536;
const MAX_EVENT_BATCH_SIZE: usize = 10_000;
const MAX_PENDING_DIRTY_PATHS: usize = 1_000_000;

/// Recorder limits and privacy choices resolved before a child is spawned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureOptions {
    /// Terminal input retention policy. `Auto` is the safe default.
    pub record_input: InputRecordingPolicy,
    /// Whether the run metadata states that an explicit environment allowlist
    /// was captured by the caller.
    pub capture_environment: bool,
    /// Maximum raw bytes in one durable terminal object.
    pub terminal_chunk_size: usize,
    /// Maximum retained terminal output bytes before capture fails visibly.
    pub terminal_max_bytes: u64,
    /// Maximum unique logical file and terminal object bytes referenced by one run.
    pub max_run_bytes: u64,
    /// Maximum pending producer messages before producers block.
    pub channel_capacity: usize,
    /// Maximum ordinary events persisted in one transaction.
    pub event_batch_size: usize,
    /// Best-effort supervised process-tree polling interval.
    pub process_poll_interval: Duration,
    /// Required quiet period after the last observed metadata change.
    pub checkpoint_debounce: Duration,
    /// Minimum elapsed time between intermediate checkpoints.
    pub checkpoint_min_interval: Duration,
    /// Maximum time a continuously changing workspace may remain pending.
    pub checkpoint_max_interval: Duration,
    /// Bounded number of unique dirty-path hints retained between checkpoints.
    pub maximum_pending_dirty_paths: usize,
    /// Authoritative snapshot scan policy.
    pub snapshot: ScanOptions,
    /// PTY size used when standard input is not a terminal.
    pub fallback_terminal_size: PtySize,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            record_input: InputRecordingPolicy::Auto,
            capture_environment: false,
            terminal_chunk_size: 64 * 1024,
            terminal_max_bytes: 2 * 1024 * 1024 * 1024,
            max_run_bytes: 10 * 1024 * 1024 * 1024,
            channel_capacity: 64,
            event_batch_size: 32,
            process_poll_interval: Duration::from_millis(250),
            checkpoint_debounce: Duration::from_millis(750),
            checkpoint_min_interval: Duration::from_secs(2),
            checkpoint_max_interval: Duration::from_secs(60),
            maximum_pending_dirty_paths: 10_000,
            snapshot: ScanOptions::default(),
            fallback_terminal_size: PtySize::default(),
        }
    }
}

impl CaptureOptions {
    fn validate(&self) -> Result<(), CaptureError> {
        if self.terminal_chunk_size == 0 || self.terminal_chunk_size > MAX_TERMINAL_CHUNK_BYTES {
            return Err(CaptureError::InvalidOptions(format!(
                "terminal_chunk_size must be in 1..={MAX_TERMINAL_CHUNK_BYTES} bytes"
            )));
        }
        if self.terminal_max_bytes == 0 {
            return Err(CaptureError::InvalidOptions(
                "terminal_max_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.max_run_bytes == 0 {
            return Err(CaptureError::InvalidOptions(
                "max_run_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.channel_capacity == 0 || self.channel_capacity > MAX_CHANNEL_CAPACITY {
            return Err(CaptureError::InvalidOptions(format!(
                "channel_capacity must be in 1..={MAX_CHANNEL_CAPACITY}"
            )));
        }
        if self.event_batch_size == 0 || self.event_batch_size > MAX_EVENT_BATCH_SIZE {
            return Err(CaptureError::InvalidOptions(format!(
                "event_batch_size must be in 1..={MAX_EVENT_BATCH_SIZE}"
            )));
        }
        if self.process_poll_interval.is_zero() {
            return Err(CaptureError::InvalidOptions(
                "process_poll_interval must be greater than zero".to_owned(),
            ));
        }
        if self.checkpoint_debounce.is_zero()
            || self.checkpoint_min_interval.is_zero()
            || self.checkpoint_max_interval.is_zero()
        {
            return Err(CaptureError::InvalidOptions(
                "checkpoint intervals must be greater than zero".to_owned(),
            ));
        }
        if self.checkpoint_min_interval > self.checkpoint_max_interval {
            return Err(CaptureError::InvalidOptions(
                "checkpoint_min_interval must not exceed checkpoint_max_interval".to_owned(),
            ));
        }
        if self.checkpoint_debounce > self.checkpoint_max_interval {
            return Err(CaptureError::InvalidOptions(
                "checkpoint_debounce must not exceed checkpoint_max_interval".to_owned(),
            ));
        }
        if self.maximum_pending_dirty_paths == 0
            || self.maximum_pending_dirty_paths > MAX_PENDING_DIRTY_PATHS
        {
            return Err(CaptureError::InvalidOptions(format!(
                "maximum_pending_dirty_paths must be in 1..={MAX_PENDING_DIRTY_PATHS}"
            )));
        }
        if self.snapshot.max_file_size == 0 {
            return Err(CaptureError::InvalidOptions(
                "snapshot max_file_size must be greater than zero".to_owned(),
            ));
        }
        if self.fallback_terminal_size.rows == 0 || self.fallback_terminal_size.columns == 0 {
            return Err(CaptureError::InvalidOptions(
                "fallback terminal rows and columns must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }

    fn policy(&self) -> CapturePolicy {
        CapturePolicy {
            record_input: self.record_input,
            capture_environment: self.capture_environment,
        }
    }
}

/// Everything needed to record one generic terminal command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureRequest {
    /// Existing source directory. It is cloned and never executed in directly.
    pub source_workspace: PathBuf,
    /// Executable name or path.
    pub command: OsString,
    /// Exact child arguments, excluding the executable.
    pub arguments: Vec<OsString>,
    /// Explicit environment additions or overrides. The normal process
    /// environment remains inherited by the PTY implementation.
    pub extra_environment: Vec<(OsString, OsString)>,
    /// Optional path exported to the child as `REWIND_BIN`, useful for local
    /// marker commands in deterministic fixtures.
    pub rewind_binary: Option<PathBuf>,
    /// Durable parent checkpoint when this execution is a fork.
    pub parent: Option<RunParent>,
    /// Logical branch identity. A fresh identity is generated when absent.
    pub branch_id: Option<BranchId>,
    /// Effective bounded capture policy.
    pub options: CaptureOptions,
}

impl CaptureRequest {
    /// Constructs a generic isolated capture using safe defaults.
    pub fn new(source_workspace: impl Into<PathBuf>, command: impl Into<OsString>) -> Self {
        Self {
            source_workspace: source_workspace.into(),
            command: command.into(),
            arguments: Vec::new(),
            extra_environment: Vec::new(),
            rewind_binary: None,
            parent: None,
            branch_id: None,
            options: CaptureOptions::default(),
        }
    }
}

/// Durable evidence returned after orderly recorder finalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureOutcome {
    /// Recorded run identity.
    pub run_id: RunId,
    /// Isolated workspace retained for later inspection.
    pub workspace_root: PathBuf,
    /// Copy strategy actually used to isolate the workspace.
    pub clone_strategy: CloneStrategy,
    /// Initial workspace identity.
    pub initial_snapshot: SnapshotId,
    /// Authoritative final workspace identity.
    pub final_snapshot: SnapshotId,
    /// Final lifecycle state.
    pub status: RunStatus,
    /// Root process result, to be propagated by the CLI when appropriate.
    pub exit_status: ProcessExitStatus,
    /// Number of committed initial, manual, and final checkpoints.
    pub checkpoint_count: u64,
    /// Exact raw terminal output bytes retained.
    pub terminal_output_bytes: u64,
}

/// Failure to prepare, supervise, persist, or finalize a capture.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// The child executable was empty or not representable in durable UTF-8 metadata.
    #[error("command must be nonempty valid UTF-8")]
    InvalidCommand,
    /// One command argument was not representable in durable UTF-8 metadata.
    #[error("command argument {index} is not valid UTF-8")]
    InvalidArgument {
        /// Zero-based argument position.
        index: usize,
    },
    /// A configured recorder bound is invalid.
    #[error("invalid capture options: {0}")]
    InvalidOptions(String),
    /// Rewind storage would be captured recursively with the source workspace.
    #[error("Rewind store {store} must not be equal to or beneath source workspace {workspace}")]
    RecursiveStore {
        /// Resolved store candidate.
        store: PathBuf,
        /// Canonical source workspace.
        workspace: PathBuf,
    },
    /// The store path or source workspace could not be canonicalized.
    #[error("cannot {operation} {path}: {source}")]
    Io {
        /// Failed filesystem operation.
        operation: &'static str,
        /// Relevant path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// Workspace isolation failed.
    #[error("cannot isolate workspace: {0}")]
    Workspace(#[from] FileSystemError),
    /// Pseudoterminal setup, I/O, or child lifecycle handling failed.
    #[error("terminal supervision failed: {0}")]
    Pty(#[from] PtyError),
    /// Parent terminal inspection or restoration failed.
    #[error("parent terminal operation failed: {0}")]
    Terminal(#[from] TerminalError),
    /// Terminal restoration failed while another capture failure was already active.
    #[error("{primary}; additionally failed to restore the parent terminal: {restore}")]
    TerminalRestoreAfterFailure {
        /// Original capture failure retained as the primary cause.
        primary: Box<CaptureError>,
        /// Failure returned by the explicit terminal restoration attempt.
        restore: TerminalError,
    },
    /// Metadata or content-addressed persistence failed.
    #[error("recording persistence failed: {0}")]
    Store(#[from] StoreError),
    /// An authoritative workspace scan failed.
    #[error("workspace checkpoint failed: {0}")]
    Snapshot(#[source] Box<SnapshotError>),
    /// The bounded producer channel stopped before orderly shutdown.
    #[error("recorder producer {producer} failed: {message}")]
    Producer {
        /// Producer role.
        producer: &'static str,
        /// Underlying non-secret diagnostic.
        message: String,
    },
    /// A worker thread panicked instead of reporting a typed failure.
    #[error("recorder worker {0} panicked")]
    WorkerPanicked(&'static str),
    /// A wall-clock or monotonic value exceeded the durable integer range.
    #[error("recording clock value is outside the durable range")]
    ClockOutOfRange,
    /// Terminal output reached the configured hard bound.
    #[error("terminal output reached the configured {maximum}-byte limit")]
    TerminalOutputLimit {
        /// Configured output byte ceiling.
        maximum: u64,
    },
    /// Unique logical objects referenced by this run reached the configured bound.
    #[error("run object storage reached the configured {maximum}-byte limit")]
    RunStorageLimit {
        /// Configured unique logical object byte ceiling.
        maximum: u64,
    },
    /// Control-socket creation or serving failed.
    #[error("local control socket failed: {0}")]
    Control(String),
    /// A process identifier supplied by the PTY was zero or out of range.
    #[error("PTY returned an invalid root process ID")]
    InvalidProcessId,
    /// The event sequence exhausted its nonzero integer range.
    #[error("run exhausted event sequence numbers")]
    EventSequenceExhausted,
    /// A constructed typed event violated a durable domain invariant.
    #[error("recorder constructed an invalid event: {0}")]
    InvalidEvent(String),
    /// Excluded content could not be removed from the retained workspace.
    #[error("could not remove {failed} excluded path(s) from the retained workspace: {source}")]
    PrivacyCleanup {
        /// Number of excluded paths whose removal failed.
        failed: usize,
        /// First operating-system removal error.
        #[source]
        source: std::io::Error,
    },
}

impl From<SnapshotError> for CaptureError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(Box::new(error))
    }
}

/// Opens a writable store and records one isolated execution.
pub fn capture(
    store_root: impl AsRef<std::path::Path>,
    request: CaptureRequest,
) -> Result<CaptureOutcome, CaptureError> {
    preflight_store_location(store_root.as_ref(), &request.source_workspace)?;
    let mut store = rewind_store::Store::open(store_root)?;
    capture_with_store(&mut store, request)
}

/// Records one execution using an already-open writable store.
///
/// This entry point lets fork orchestration resolve and materialize a parent
/// checkpoint without attempting to acquire the store writer lock twice.
pub fn capture_with_store(
    store: &mut rewind_store::Store,
    request: CaptureRequest,
) -> Result<CaptureOutcome, CaptureError> {
    recorder::capture_with_store(store, request)
}

fn preflight_store_location(
    store: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<(), CaptureError> {
    let workspace = std::fs::canonicalize(workspace).map_err(|source| CaptureError::Io {
        operation: "canonicalize source workspace",
        path: workspace.to_path_buf(),
        source,
    })?;
    let store = resolve_future_path(store)?;
    if store == workspace || store.starts_with(&workspace) {
        return Err(CaptureError::RecursiveStore { store, workspace });
    }
    Ok(())
}

fn resolve_future_path(path: &std::path::Path) -> Result<PathBuf, CaptureError> {
    if path.exists() {
        return std::fs::canonicalize(path).map_err(|source| CaptureError::Io {
            operation: "canonicalize store",
            path: path.to_path_buf(),
            source,
        });
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| CaptureError::Io {
                operation: "resolve current directory for",
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| CaptureError::Io {
            operation: "resolve future store path",
            path: absolute.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no existing ancestor",
            ),
        })?;
        suffix.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| CaptureError::Io {
            operation: "resolve future store path",
            path: absolute.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no existing ancestor",
            ),
        })?;
    }
    let mut resolved = std::fs::canonicalize(ancestor).map_err(|source| CaptureError::Io {
        operation: "canonicalize store ancestor",
        path: ancestor.to_path_buf(),
        source,
    })?;
    for component in suffix.into_iter().rev() {
        if component == "." {
            continue;
        }
        if component == ".." {
            resolved.pop();
        } else {
            resolved.push(component);
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_store_is_rejected_before_it_is_created() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = workspace.join("nested/store");
        assert!(matches!(
            preflight_store_location(&store, &workspace),
            Err(CaptureError::RecursiveStore { .. })
        ));
        assert!(!store.exists());
    }
}
