use std::io;
use std::os::fd::RawFd;
use thiserror::Error;

/// Errors changing or inspecting a terminal.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// A termios operation failed.
    #[error("terminal {operation} failed: {source}")]
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },
}

/// RAII guard that restores the exact prior terminal attributes on drop.
#[derive(Debug)]
pub struct TerminalModeGuard {
    fd: RawFd,
    original: libc::termios,
    active: bool,
}

impl TerminalModeGuard {
    /// Changes `fd` to raw mode and returns a restoration guard.
    pub fn enter_raw(fd: RawFd) -> Result<Self, TerminalError> {
        let original = get_attributes(fd)?;
        let mut raw = original;
        // SAFETY: `raw` is a fully initialized termios value owned by this
        // function. `cfmakeraw` mutates only that value.
        unsafe { libc::cfmakeraw(&mut raw) };
        set_attributes(fd, &raw, "enable raw mode")?;
        Ok(Self {
            fd,
            original,
            active: true,
        })
    }

    /// Restores terminal attributes immediately. Drop remains idempotent.
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if self.active {
            set_attributes(self.fd, &self.original, "restore mode")?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Returns whether input echo is currently enabled for a terminal descriptor.
pub fn terminal_echo_enabled(fd: RawFd) -> Result<bool, TerminalError> {
    Ok(get_attributes(fd)?.c_lflag & libc::ECHO != 0)
}

pub(crate) fn set_terminal_echo(fd: RawFd, enabled: bool) -> Result<(), TerminalError> {
    let mut attributes = get_attributes(fd)?;
    if enabled {
        attributes.c_lflag |= libc::ECHO;
    } else {
        attributes.c_lflag &= !libc::ECHO;
    }
    set_attributes(fd, &attributes, "set echo mode")
}

/// Reads the current kernel terminal dimensions.
pub fn terminal_size(fd: RawFd) -> Result<crate::PtySize, TerminalError> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    // SAFETY: `size` is correctly aligned writable storage and TIOCGWINSZ
    // initializes it on success without retaining the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return Err(TerminalError::Io {
            operation: "read window size",
            source: io::Error::last_os_error(),
        });
    }
    // SAFETY: ioctl returned success and initialized `size`.
    let size = unsafe { size.assume_init() };
    if size.ws_row == 0 || size.ws_col == 0 {
        return Err(TerminalError::Io {
            operation: "read window size",
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal reported a zero dimension",
            ),
        });
    }
    Ok(crate::PtySize {
        rows: size.ws_row,
        columns: size.ws_col,
        pixel_width: size.ws_xpixel,
        pixel_height: size.ws_ypixel,
    })
}

fn get_attributes(fd: RawFd) -> Result<libc::termios, TerminalError> {
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `attributes` is valid writable storage and `fd` is only passed to
    // the OS. Success means the storage was initialized.
    if unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) } != 0 {
        return Err(TerminalError::Io {
            operation: "read mode",
            source: io::Error::last_os_error(),
        });
    }
    // SAFETY: tcgetattr returned success and initialized `attributes`.
    Ok(unsafe { attributes.assume_init() })
}

fn set_attributes(
    fd: RawFd,
    attributes: &libc::termios,
    operation: &'static str,
) -> Result<(), TerminalError> {
    // SAFETY: `attributes` points to a valid initialized termios value and the
    // OS only borrows it for this call.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, attributes) } != 0 {
        return Err(TerminalError::Io {
            operation,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[test]
    fn echo_toggle_and_guard_restore_terminal_attributes() {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: pointers refer to initialized descriptor slots; null name and
        // settings request the documented defaults. Returned descriptors become
        // owned `File`s immediately after success.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0);
        // SAFETY: openpty returned two newly owned valid descriptors.
        let _master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: openpty returned two newly owned valid descriptors.
        let _slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let before = get_attributes(slave).unwrap();
        set_terminal_echo(master, false).unwrap();
        assert!(!terminal_echo_enabled(slave).unwrap());
        set_terminal_echo(master, before.c_lflag & libc::ECHO != 0).unwrap();
        {
            let mut guard = TerminalModeGuard::enter_raw(slave).unwrap();
            let during = get_attributes(slave).unwrap();
            assert_eq!(during.c_lflag & libc::ECHO, 0);
            guard.restore().unwrap();
            guard.restore().unwrap();
        }
        let after = get_attributes(slave).unwrap();
        assert_eq!(before.c_iflag, after.c_iflag);
        assert_eq!(before.c_oflag, after.c_oflag);
        assert_eq!(before.c_cflag, after.c_cflag);
        // macOS may set PENDIN while the line discipline reprocesses queued
        // input; compare the user-configurable behavior rather than that
        // transient kernel status bit.
        assert_eq!(
            before.c_lflag & !libc::PENDIN,
            after.c_lflag & !libc::PENDIN
        );
    }
}
