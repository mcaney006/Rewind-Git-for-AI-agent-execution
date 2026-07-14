use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rewind_platform::ExclusiveFileLock;

use crate::{Result, StoreError};

const LOCK_VERSION: u8 = 1;
const MAX_LOCK_BYTES: u64 = 16 * 1024;
static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Human-readable diagnostics for a held store writer lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// Owning process ID when parseable.
    pub pid: Option<u32>,
    /// Lock creation time when parseable.
    pub created_unix_ms: Option<u64>,
    /// Ownership token used to prevent deleting a replacement lock.
    pub token: Option<String>,
    /// Bounded original contents for diagnostics.
    pub raw: String,
}

impl LockInfo {
    fn parse(raw: String) -> Self {
        let mut info = Self {
            pid: None,
            created_unix_ms: None,
            token: None,
            raw,
        };
        for line in info.raw.lines() {
            if let Some(value) = line.strip_prefix("pid=") {
                info.pid = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("created_unix_ms=") {
                info.created_unix_ms = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("token=") {
                info.token = Some(value.to_owned());
            }
        }
        info
    }
}

#[derive(Debug)]
pub(crate) struct StoreLock {
    _guard: ExclusiveFileLock,
}

impl StoreLock {
    pub(crate) fn acquire(store_root: &Path) -> Result<Self> {
        let path = store_root.join("writer.lock");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| StoreError::Clock { source })?;
        let sequence = LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let token = format!("{}-{}-{sequence}", std::process::id(), now.as_nanos());
        let body = format!(
            "version={LOCK_VERSION}\npid={}\ncreated_unix_ms={}\ntoken={token}\n",
            std::process::id(),
            now.as_millis()
        );

        let mut guard = match ExclusiveFileLock::try_open(&path) {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                let owner = inspect(&path).unwrap_or_else(|error| LockInfo {
                    pid: None,
                    created_unix_ms: None,
                    token: None,
                    raw: format!("unreadable lock: {error}"),
                });
                return Err(StoreError::Locked { path, owner });
            }
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "acquire store writer lock",
                    path,
                    source,
                });
            }
        };

        let file = guard.file_mut();
        let write_result = file
            .set_len(0)
            .and_then(|()| file.rewind())
            .and_then(|()| file.write_all(body.as_bytes()))
            .and_then(|()| file.sync_all());
        if let Err(source) = write_result {
            return Err(StoreError::Io {
                operation: "persist store writer lock",
                path,
                source,
            });
        }
        Ok(Self { _guard: guard })
    }
}

pub(crate) fn inspect(path: &Path) -> Result<LockInfo> {
    let mut file = File::open(path).map_err(|source| StoreError::Io {
        operation: "open store writer lock",
        path: path.to_path_buf(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| StoreError::Io {
            operation: "read store writer lock metadata",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if size > MAX_LOCK_BYTES {
        return Err(StoreError::MalformedLock {
            path: path.to_path_buf(),
            message: format!("lock is {size} bytes; maximum is {MAX_LOCK_BYTES}"),
        });
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|source| StoreError::Io {
            operation: "read store writer lock",
            path: path.to_path_buf(),
            source,
        })?;
    Ok(LockInfo::parse(raw))
}

pub(crate) fn inspect_held(path: &Path) -> Result<Option<LockInfo>> {
    match ExclusiveFileLock::try_open_existing(path) {
        Ok(Some(_available)) => Ok(None),
        Ok(None) => inspect(path).map(Some),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            operation: "inspect store writer lock ownership",
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_diagnostics_without_trusting_them() {
        let info = LockInfo::parse("pid=42\ncreated_unix_ms=nope\ntoken=x\n".to_owned());
        assert_eq!(info.pid, Some(42));
        assert_eq!(info.created_unix_ms, None);
        assert_eq!(info.token.as_deref(), Some("x"));
    }
}
