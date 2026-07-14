use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// An exclusive nonblocking advisory lock held by one open file description.
///
/// Closing this guard releases the kernel-owned lock, including after process
/// termination. The lock file itself may remain for bounded diagnostics.
#[derive(Debug)]
pub struct ExclusiveFileLock {
    file: File,
}

impl ExclusiveFileLock {
    /// Opens or creates `path` without following a final symlink and attempts
    /// to acquire its exclusive kernel lock.
    pub fn try_open(path: &Path) -> io::Result<Option<Self>> {
        Self::try_open_with(path, true)
    }

    /// Opens an existing path without following a final symlink and attempts
    /// to acquire its exclusive kernel lock.
    pub fn try_open_existing(path: &Path) -> io::Result<Option<Self>> {
        Self::try_open_with(path, false)
    }

    fn try_open_with(path: &Path, create: bool) -> io::Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "advisory lock path is not a regular file",
            ));
        }
        let Some(lock) = Self::try_acquire(file)? else {
            return Ok(None);
        };
        if create {
            lock.file
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(Some(lock))
    }

    /// Attempts to lock `file`, returning `None` when another open description
    /// currently owns the lock.
    pub fn try_acquire(file: File) -> io::Result<Option<Self>> {
        // SAFETY: `file` owns a live descriptor for the duration of the call;
        // `flock` neither retains pointers nor transfers descriptor ownership.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Some(Self { file }));
        }
        let error = io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            Ok(None)
        } else {
            Err(error)
        }
    }

    /// Mutably borrows the locked file for diagnostic metadata updates.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_lock_rejects_a_second_owner_and_releases_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lock");
        let owner = ExclusiveFileLock::try_open(&path).unwrap().unwrap();
        assert!(ExclusiveFileLock::try_open(&path).unwrap().is_none());
        drop(owner);
        assert!(ExclusiveFileLock::try_open(&path).unwrap().is_some());
    }
}
