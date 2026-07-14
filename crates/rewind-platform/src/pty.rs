use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, native_pty_system};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use thiserror::Error;

/// Pseudoterminal dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtySize {
    /// Character rows.
    pub rows: u16,
    /// Character columns.
    pub columns: u16,
    /// Pixel width, or zero when unknown.
    pub pixel_width: u16,
    /// Pixel height, or zero when unknown.
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: 24,
            columns: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl From<PtySize> for portable_pty::PtySize {
    fn from(value: PtySize) -> Self {
        Self {
            rows: value.rows,
            cols: value.columns,
            pixel_width: value.pixel_width,
            pixel_height: value.pixel_height,
        }
    }
}

/// A completed child status independent of the PTY implementation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChildExit {
    /// Numeric exit code. Signal exits use the portable PTY fallback code.
    pub code: u32,
    /// Signal description when termination was signal-driven.
    pub signal: Option<String>,
    /// Numeric Unix signal when the platform description can be resolved.
    pub signal_number: Option<i32>,
}

/// Errors opening, spawning, or operating a PTY.
#[derive(Debug, Error)]
pub enum PtyError {
    /// Opening the native PTY failed.
    #[error("could not open native pseudoterminal: {0}")]
    Open(String),
    /// Spawning the child failed.
    #[error("could not spawn {program:?}: {message}")]
    Spawn {
        /// Program requested.
        program: OsString,
        /// PTY implementation error.
        message: String,
    },
    /// Creating a reader or writer failed.
    #[error("could not open PTY {operation}: {message}")]
    Stream {
        /// Reader/writer operation.
        operation: &'static str,
        /// PTY implementation error.
        message: String,
    },
    /// Waiting, polling, or killing failed.
    #[error("PTY child {operation} failed: {source}")]
    Child {
        /// Child operation.
        operation: &'static str,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Resizing the PTY failed.
    #[error("could not resize PTY: {0}")]
    Resize(String),
}

/// Owned pieces of a spawned pseudoterminal process.
pub struct PtyProcess {
    /// Master used for resize and echo-state observation.
    pub master: PtyMaster,
    /// Stream of raw output bytes from the child PTY.
    pub reader: Box<dyn Read + Send>,
    /// Stream that forwards input bytes to the child PTY.
    pub writer: Box<dyn Write + Send>,
    /// Child lifecycle handle.
    pub child: PtyChild,
}

/// Safe master-side PTY operations.
pub struct PtyMaster {
    inner: Box<dyn MasterPty + Send>,
}

/// Independent descriptor used to sample slave echo beside an input read.
pub struct PtyEchoProbe {
    descriptor: OwnedFd,
}

impl PtyMaster {
    /// Updates the child terminal size and lets the kernel deliver resize notification.
    pub fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        self.inner
            .resize(size.into())
            .map_err(|error| PtyError::Resize(error.to_string()))
    }

    /// Returns whether the slave terminal currently has input echo enabled.
    #[must_use]
    pub fn echo_enabled(&self) -> Option<bool> {
        self.inner
            .as_raw_fd()
            .and_then(|fd| crate::terminal_echo_enabled(fd).ok())
    }

    /// Duplicates the native master descriptor for a dedicated input producer.
    pub fn try_clone_echo_probe(&self) -> Result<PtyEchoProbe, PtyError> {
        let descriptor = self.inner.as_raw_fd().ok_or_else(|| PtyError::Stream {
            operation: "echo probe",
            message: "native PTY descriptor is unavailable".to_owned(),
        })?;
        // SAFETY: `descriptor` is live for this call. F_DUPFD_CLOEXEC returns a
        // new owned descriptor that shares the same terminal state.
        let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(PtyError::Stream {
                operation: "echo probe",
                message: io::Error::last_os_error().to_string(),
            });
        }
        // SAFETY: fcntl returned a distinct descriptor owned by this wrapper.
        let descriptor = unsafe { OwnedFd::from_raw_fd(duplicate) };
        Ok(PtyEchoProbe { descriptor })
    }

    /// Enables or disables slave-side input echo through the master descriptor.
    pub fn set_echo_enabled(&self, enabled: bool) -> Result<(), PtyError> {
        let fd = self.inner.as_raw_fd().ok_or_else(|| PtyError::Stream {
            operation: "echo mode",
            message: "native PTY descriptor is unavailable".to_owned(),
        })?;
        crate::terminal::set_terminal_echo(fd, enabled).map_err(|error| PtyError::Stream {
            operation: "echo mode",
            message: error.to_string(),
        })
    }
}

impl PtyEchoProbe {
    /// Returns the echo state at the instant this method reaches the kernel.
    #[must_use]
    pub fn echo_enabled(&self) -> Option<bool> {
        crate::terminal_echo_enabled(self.descriptor.as_raw_fd()).ok()
    }
}

/// Safe lifecycle operations for a PTY child.
pub struct PtyChild {
    inner: Box<dyn Child + Send + Sync>,
}

impl PtyChild {
    /// Returns the child process ID when supplied by the platform.
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.inner.process_id()
    }

    /// Polls without blocking.
    pub fn try_wait(&mut self) -> Result<Option<ChildExit>, PtyError> {
        self.inner
            .try_wait()
            .map(|status| status.map(child_exit))
            .map_err(|source| PtyError::Child {
                operation: "poll",
                source,
            })
    }

    /// Waits for and reaps the child.
    pub fn wait(&mut self) -> Result<ChildExit, PtyError> {
        self.inner
            .wait()
            .map(child_exit)
            .map_err(|source| PtyError::Child {
                operation: "wait",
                source,
            })
    }

    /// Requests termination of the child through the portable PTY implementation.
    pub fn kill(&mut self) -> Result<(), PtyError> {
        self.inner.kill().map_err(|source| PtyError::Child {
            operation: "kill",
            source,
        })
    }

    /// Forwards one Unix signal to the child's PTY process group.
    pub fn signal_process_group(&self, signal: i32) -> Result<(), PtyError> {
        let pid = self.process_group_id()?;
        // SAFETY: `spawn_pty` creates the child as a session and process-group
        // leader. A negative PID targets that group, and `kill` borrows no memory.
        let result = unsafe { libc::kill(-pid, signal) };
        if result == 0 {
            return Ok(());
        }
        let source = io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ESRCH) {
            // The child may have exited between the coordinator poll and signal.
            return Ok(());
        }
        Err(PtyError::Child {
            operation: "signal process group",
            source,
        })
    }

    /// Checks whether any process still belongs to the spawned PTY process group.
    pub fn process_group_exists(&self) -> Result<bool, PtyError> {
        let pid = self.process_group_id()?;
        // SAFETY: Signal zero performs only the documented existence/permission
        // check for the negative-PID process group and borrows no memory.
        let result = unsafe { libc::kill(-pid, 0) };
        if result == 0 {
            return Ok(true);
        }
        let source = io::Error::last_os_error();
        match source.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(PtyError::Child {
                operation: "inspect process group",
                source,
            }),
        }
    }

    /// Force-terminates the complete PTY process group.
    pub fn kill_process_group(&self) -> Result<(), PtyError> {
        self.signal_process_group(libc::SIGKILL)
    }

    /// Creates a thread-safe termination handle.
    #[must_use]
    pub fn killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        self.inner.clone_killer()
    }

    fn process_group_id(&self) -> Result<i32, PtyError> {
        let pid = self.process_id().ok_or_else(|| PtyError::Child {
            operation: "inspect process group",
            source: io::Error::new(io::ErrorKind::InvalidData, "child PID is unavailable"),
        })?;
        i32::try_from(pid).map_err(|_| PtyError::Child {
            operation: "inspect process group",
            source: io::Error::new(io::ErrorKind::InvalidData, "child PID exceeds i32"),
        })
    }
}

/// Opens a native PTY and spawns one command inside it.
pub fn spawn_pty(
    program: &OsStr,
    arguments: &[OsString],
    working_directory: &Path,
    environment: &[(OsString, OsString)],
    size: PtySize,
) -> Result<PtyProcess, PtyError> {
    let pair = native_pty_system()
        .openpty(size.into())
        .map_err(|error| PtyError::Open(error.to_string()))?;
    let mut command = CommandBuilder::new(program);
    command.args(arguments);
    command.cwd(working_directory);
    for (key, value) in environment {
        command.env(key, value);
    }
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| PtyError::Spawn {
            program: program.to_owned(),
            message: error.to_string(),
        })?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| PtyError::Stream {
            operation: "reader",
            message: error.to_string(),
        })?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| PtyError::Stream {
            operation: "writer",
            message: error.to_string(),
        })?;
    Ok(PtyProcess {
        master: PtyMaster { inner: pair.master },
        reader,
        writer,
        child: PtyChild { inner: child },
    })
}

fn child_exit(status: portable_pty::ExitStatus) -> ChildExit {
    let signal = status.signal().map(ToOwned::to_owned);
    ChildExit {
        code: status.exit_code(),
        signal_number: signal.as_deref().and_then(signal_number),
        signal,
    }
}

fn signal_number(description: &str) -> Option<i32> {
    (1..=64).find(|signal| {
        // SAFETY: `strsignal` returns either null or a process-owned NUL-terminated
        // description. We copy/compare it before the next call can reuse storage.
        let pointer = unsafe { libc::strsignal(*signal) };
        if pointer.is_null() {
            return false;
        }
        // SAFETY: A non-null `strsignal` result is a valid NUL-terminated C string.
        unsafe { std::ffi::CStr::from_ptr(pointer) }.to_string_lossy() == description
    })
}

impl std::fmt::Debug for PtyMaster {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PtyMaster").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PtyEchoProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtyEchoProbe")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PtyChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtyChild")
            .field("pid", &self.process_id())
            .finish()
    }
}

impl std::fmt::Debug for PtyProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtyProcess")
            .field("master", &self.master)
            .field("child", &self.child)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn forwards_interrupt_to_the_pty_process_group() {
        let temporary = tempfile::tempdir().unwrap();
        let mut process = spawn_pty(
            OsStr::new("/bin/sh"),
            &[
                OsString::from("-c"),
                OsString::from("trap 'exit 42' INT; printf ready; while :; do :; done"),
            ],
            temporary.path(),
            &[],
            PtySize::default(),
        )
        .unwrap();
        let mut ready = [0_u8; 5];
        process.reader.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");

        process.child.signal_process_group(libc::SIGINT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(exit) = process.child.try_wait().unwrap() {
                assert_eq!(exit.code, 42);
                break;
            }
            if Instant::now() >= deadline {
                process.child.kill_process_group().unwrap();
                let _ = process.child.wait();
                panic!("child did not handle SIGINT within two seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn resolves_numeric_signal_from_portable_description() {
        // SAFETY: `strsignal(SIGTERM)` returns a static NUL-terminated description.
        let pointer = unsafe { libc::strsignal(libc::SIGTERM) };
        assert!(!pointer.is_null());
        // SAFETY: The non-null pointer is valid until the next `strsignal` call.
        let description = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(signal_number(&description), Some(libc::SIGTERM));
    }

    #[test]
    fn independent_echo_probe_tracks_slave_mode() {
        let temporary = tempfile::tempdir().unwrap();
        let mut process = spawn_pty(
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("sleep 30")],
            temporary.path(),
            &[],
            PtySize::default(),
        )
        .unwrap();
        let probe = process.master.try_clone_echo_probe().unwrap();

        process.master.set_echo_enabled(false).unwrap();
        assert_eq!(probe.echo_enabled(), Some(false));
        process.master.set_echo_enabled(true).unwrap();
        assert_eq!(probe.echo_enabled(), Some(true));

        process.child.kill_process_group().unwrap();
        process.child.wait().unwrap();
    }
}
