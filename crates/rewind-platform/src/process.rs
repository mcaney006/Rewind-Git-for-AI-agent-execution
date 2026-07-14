use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(target_os = "macos")]
use std::ffi::CStr;
#[cfg(target_os = "linux")]
use std::fs;
use std::io;
#[cfg(target_os = "macos")]
use std::mem::size_of;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use thiserror::Error;

/// Observed process metadata. It is advisory rather than authoritative.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Observed parent process identifier.
    pub parent_pid: Option<u32>,
    /// Executable path when the platform exposes it.
    pub executable: Option<PathBuf>,
    /// Short command name.
    pub command: String,
}

/// Errors while observing the supervised process tree.
#[derive(Debug, Error)]
pub enum ProcessObservationError {
    /// Reading Linux procfs failed.
    #[error("process observation failed for {path}: {source}")]
    Io {
        /// Procfs or executable path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The macOS process listing command failed.
    #[error("process observation command failed: {0}")]
    Command(String),
    /// Platform output was malformed.
    #[error("malformed process metadata: {0}")]
    Malformed(String),
}

/// Observes the root process and descendants visible at this instant.
pub fn supervised_processes(root_pid: u32) -> Result<Vec<ProcessInfo>, ProcessObservationError> {
    let all = all_processes()?;
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for process in all.values() {
        if let Some(parent) = process.parent_pid {
            children.entry(parent).or_default().push(process.pid);
        }
    }
    let mut queue = VecDeque::from([root_pid]);
    let mut selected = BTreeSet::new();
    while let Some(pid) = queue.pop_front() {
        if !selected.insert(pid) {
            continue;
        }
        if let Some(descendants) = children.get(&pid) {
            queue.extend(descendants);
        }
    }
    Ok(selected
        .into_iter()
        .filter_map(|pid| all.get(&pid).cloned())
        .collect())
}

#[cfg(target_os = "macos")]
fn all_processes() -> Result<BTreeMap<u32, ProcessInfo>, ProcessObservationError> {
    let mut processes = BTreeMap::new();
    for pid in macos_pids()? {
        let mut info = std::mem::MaybeUninit::<ProcBsdShortInfo>::uninit();
        let size = i32::try_from(size_of::<ProcBsdShortInfo>()).map_err(|_| {
            ProcessObservationError::Malformed("proc info structure is too large".to_owned())
        })?;
        // SAFETY: `info` is correctly aligned writable storage of `size` bytes.
        // The flavor and layout match the locally installed macOS SDK.
        let read = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDT_SHORTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            continue;
        }
        // SAFETY: proc_pidinfo returned the complete requested structure size.
        let info = unsafe { info.assume_init() };
        if info.pid == 0 {
            continue;
        }
        let command = c_char_array_to_string(&info.command);
        let executable = macos_pid_path(pid);
        processes.insert(
            info.pid,
            ProcessInfo {
                pid: info.pid,
                parent_pid: Some(info.parent_pid),
                executable,
                command,
            },
        );
    }
    Ok(processes)
}

#[cfg(target_os = "macos")]
const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;

#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcBsdShortInfo {
    pid: u32,
    parent_pid: u32,
    process_group_id: u32,
    status: u32,
    command: [libc::c_char; 16],
    flags: u32,
    uid: u32,
    gid: u32,
    real_uid: u32,
    real_gid: u32,
    saved_uid: u32,
    saved_gid: u32,
    reserved: u32,
}

#[cfg(target_os = "macos")]
// SAFETY: These declarations match the macOS libproc headers; every call below
// supplies buffers with the declared size and checks the returned byte count.
unsafe extern "C" {
    fn proc_listallpids(buffer: *mut libc::c_void, buffer_size: libc::c_int) -> libc::c_int;
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        argument: u64,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffer_size: u32) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn macos_pids() -> Result<Vec<libc::c_int>, ProcessObservationError> {
    // SAFETY: A null buffer with zero size is the documented sizing query.
    let count = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(ProcessObservationError::Command(
            io::Error::last_os_error().to_string(),
        ));
    }
    let count = usize::try_from(count)
        .map_err(|_| ProcessObservationError::Malformed("negative process count".to_owned()))?;
    let capacity = count.saturating_add(256).min(1_000_000);
    let mut pids = vec![0_i32; capacity];
    let bytes = capacity
        .checked_mul(size_of::<libc::c_int>())
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| {
            ProcessObservationError::Malformed("process list exceeds platform bounds".to_owned())
        })?;
    // SAFETY: `pids` owns `bytes` writable bytes and remains alive during the call.
    let populated = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if populated < 0 {
        return Err(ProcessObservationError::Command(
            io::Error::last_os_error().to_string(),
        ));
    }
    let populated = usize::try_from(populated).map_err(|_| {
        ProcessObservationError::Malformed("negative populated process count".to_owned())
    })?;
    pids.truncate(populated.min(pids.len()));
    pids.retain(|pid| *pid > 0);
    Ok(pids)
}

#[cfg(target_os = "macos")]
fn macos_pid_path(pid: libc::c_int) -> Option<PathBuf> {
    const MAX_PATH_BYTES: usize = 16 * 1024;
    let mut bytes = vec![0_u8; MAX_PATH_BYTES];
    // SAFETY: `bytes` is writable for the declared size and remains alive.
    let length = unsafe { proc_pidpath(pid, bytes.as_mut_ptr().cast(), MAX_PATH_BYTES as u32) };
    let length = usize::try_from(length).ok()?;
    if length == 0 || length > bytes.len() {
        return None;
    }
    bytes.truncate(length);
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(target_os = "macos")]
fn c_char_array_to_string(value: &[libc::c_char]) -> String {
    let pointer = value.as_ptr();
    // The kernel's fixed command buffer is documented as NUL-terminated when
    // shorter than MAXCOMLEN. Ensure termination even for a full buffer.
    let length = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    if length < value.len() {
        // SAFETY: `pointer` addresses this live array and the prior scan found a
        // terminating NUL within its bounds.
        return unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
    }
    value
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>()
        .escape_ascii()
        .to_string()
}

#[cfg(target_os = "linux")]
fn all_processes() -> Result<BTreeMap<u32, ProcessInfo>, ProcessObservationError> {
    let mut processes = BTreeMap::new();
    for entry in fs::read_dir("/proc").map_err(|source| ProcessObservationError::Io {
        path: PathBuf::from("/proc"),
        source,
    })? {
        let entry = entry.map_err(|source| ProcessObservationError::Io {
            path: PathBuf::from("/proc"),
            source,
        })?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat_path = entry.path().join("stat");
        let Ok(stat) = fs::read_to_string(&stat_path) else {
            continue;
        };
        let Some((parent_pid, command)) = parse_linux_stat(&stat) else {
            continue;
        };
        let executable = fs::read_link(entry.path().join("exe")).ok();
        processes.insert(
            pid,
            ProcessInfo {
                pid,
                parent_pid: Some(parent_pid),
                executable,
                command,
            },
        );
    }
    Ok(processes)
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(value: &str) -> Option<(u32, String)> {
    let open = value.find('(')?;
    let close = value.rfind(')')?;
    if close <= open {
        return None;
    }
    let command = value.get(open + 1..close)?.to_owned();
    let suffix = value.get(close + 1..)?.trim();
    let mut fields = suffix.split_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    Some((parent, command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_observable_as_root() {
        let current = std::process::id();
        let observed = supervised_processes(current).unwrap();
        assert_eq!(observed.first().map(|process| process.pid), Some(current));
        assert!(observed[0].command.len() <= 16 || observed[0].executable.is_some());
    }
}
