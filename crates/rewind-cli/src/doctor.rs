use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rewind_domain::RunStatus;
use rewind_platform::{PtySize, capabilities, clone_workspace, disk_space, spawn_pty};
use rewind_store::{LATEST_SCHEMA_VERSION, Store, StoreError};
use serde::Serialize;
use thiserror::Error;

const OBJECT_VERIFICATION_LOGICAL_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) healthy: bool,
    pub(crate) store_root: PathBuf,
    pub(crate) checks: Vec<DoctorCheck>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) name: &'static str,
    pub(crate) status: CheckStatus,
    pub(crate) detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Error)]
pub(crate) enum DoctorError {
    #[error("cannot inspect {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(crate) fn inspect(store_root: &Path) -> Result<DoctorReport, DoctorError> {
    let database = store_root.join("metadata.sqlite3");
    if !database.try_exists().map_err(|source| DoctorError::Io {
        path: database.clone(),
        source,
    })? {
        drop(Store::open(store_root)?);
    }
    let mut checks = Vec::new();
    let metadata = fs::metadata(store_root).map_err(|source| DoctorError::Io {
        path: store_root.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    checks.push(DoctorCheck {
        name: "storage_permissions",
        status: if mode & 0o077 == 0 {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        detail: format!("{} mode {mode:04o}", store_root.display()),
    });

    let lock = Store::inspect_writer_lock(store_root)?;
    checks.push(DoctorCheck {
        name: "writer_lock",
        status: if lock.is_some() {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        detail: lock.map_or_else(
            || "no writer currently holds the store".to_owned(),
            |owner| {
                format!(
                    "kernel lock held by PID {:?} since {:?}",
                    owner.pid, owner.created_unix_ms
                )
            },
        ),
    });

    let store = Store::open_read_only(store_root)?;
    checks.push(DoctorCheck {
        name: "database",
        status: CheckStatus::Ok,
        detail: format!("SQLite schema {LATEST_SCHEMA_VERSION} is readable"),
    });
    let interrupted = store
        .list_runs()?
        .into_iter()
        .filter(|run| run.status == RunStatus::Interrupted)
        .count();
    checks.push(DoctorCheck {
        name: "interrupted_runs",
        status: if interrupted == 0 {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        detail: format!("{interrupted} interrupted run(s) remain inspectable"),
    });
    let verification =
        store.sample_object_corruption(32, OBJECT_VERIFICATION_LOGICAL_BYTE_BUDGET)?;
    checks.push(DoctorCheck {
        name: "object_sample",
        status: if !verification.corrupt.is_empty() {
            CheckStatus::Error
        } else if verification.skipped.is_empty() {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        detail: if !verification.corrupt.is_empty() {
            format!(
                "{} of {} checked object(s) are corrupt: {}; {} object(s) skipped by the {}-byte logical read budget",
                verification.corrupt.len(),
                verification.checked,
                verification
                    .corrupt
                    .iter()
                    .map(|item| item.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                verification.skipped.len(),
                OBJECT_VERIFICATION_LOGICAL_BYTE_BUDGET,
            )
        } else if verification.skipped.is_empty() {
            format!("verified {} object(s)", verification.checked)
        } else {
            format!(
                "verified {} object(s); skipped {} object(s) after the {}-byte logical read budget was exhausted",
                verification.checked,
                verification.skipped.len(),
                OBJECT_VERIFICATION_LOGICAL_BYTE_BUDGET,
            )
        },
    });

    let space = disk_space(store_root).map_err(|source| DoctorError::Io {
        path: store_root.to_path_buf(),
        source: std::io::Error::other(source),
    })?;
    checks.push(DoctorCheck {
        name: "disk_space",
        status: if space.available < 1024 * 1024 * 1024 {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        detail: format!("{} bytes available", space.available),
    });
    let platform = capabilities();
    checks.push(DoctorCheck {
        name: "platform",
        status: CheckStatus::Ok,
        detail: format!(
            "{} {}: clone={}, watcher={}, processes={}",
            platform.operating_system,
            platform.architecture,
            platform.clone_primitive,
            platform.watcher_primitive,
            platform.process_source
        ),
    });
    checks.push(probe_clone(store_root));
    checks.push(probe_pty());
    let healthy = checks
        .iter()
        .all(|check| check.status != CheckStatus::Error);
    Ok(DoctorReport {
        healthy,
        store_root: store_root.to_path_buf(),
        checks,
    })
}

fn probe_clone(store_root: &Path) -> DoctorCheck {
    let result = (|| {
        let temporary = tempfile::Builder::new()
            .prefix("doctor-")
            .tempdir_in(store_root)?;
        let source = temporary.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("probe"), b"rewind")?;
        clone_workspace(&source, &temporary.path().join("clone")).map_err(std::io::Error::other)
    })();
    match result {
        Ok(report) => DoctorCheck {
            name: "workspace_clone",
            status: CheckStatus::Ok,
            detail: format!("selected {:?} for probe workspace", report.strategy),
        },
        Err(error) => DoctorCheck {
            name: "workspace_clone",
            status: CheckStatus::Error,
            detail: error.to_string(),
        },
    }
}

fn probe_pty() -> DoctorCheck {
    let result = std::env::current_dir()
        .map_err(|error| error.to_string())
        .and_then(|directory| {
            spawn_pty(
                OsStr::new("sh"),
                &[OsString::from("-c"), OsString::from("exit 0")],
                &directory,
                &[],
                PtySize::default(),
            )
            .map_err(|error| error.to_string())
        })
        .and_then(|mut process| process.child.wait().map_err(|error| error.to_string()));
    match result {
        Ok(exit) if exit.code == 0 => DoctorCheck {
            name: "pty",
            status: CheckStatus::Ok,
            detail: "native pseudoterminal spawned and reaped a child".to_owned(),
        },
        Ok(exit) => DoctorCheck {
            name: "pty",
            status: CheckStatus::Error,
            detail: format!("PTY probe exited with {} ({:?})", exit.code, exit.signal),
        },
        Err(error) => DoctorCheck {
            name: "pty",
            status: CheckStatus::Error,
            detail: error,
        },
    }
}
