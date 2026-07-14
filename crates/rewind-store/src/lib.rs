#![doc = "Crash-safe local metadata and immutable object persistence for Rewind."]
#![deny(missing_docs)]

mod bundle;
mod event;
mod lock;
mod maintenance;
mod object;
mod recovery;
mod run;
mod schema;
mod snapshot;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

pub use event::{MAX_TIMELINE_PAGE, TimelinePage};
pub use lock::LockInfo;
pub use maintenance::{
    DeletedObject, DeletedOrphanFile, MAX_OBJECT_PAGE, ObjectCorruption, ObjectIdPage, ObjectPage,
    ObjectRecord, ObjectVerification, ObjectVerificationSkipped, OrphanFile, OrphanFileKind,
    OrphanFilePage,
};
pub use object::{ObjectCompression, ObjectReader, ObjectStore, StoredObject};
pub use recovery::{ComparisonInput, WarningRecord};
use rewind_domain::{ObjectId, Timestamp};
pub use run::RunFinish;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
pub use schema::LATEST_SCHEMA_VERSION;
use thiserror::Error;

/// Errors preserve the failed operation and relevant path or durable identity.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A filesystem operation failed.
    #[error("cannot {operation} at {path}: {source}")]
    Io {
        /// The attempted operation.
        operation: &'static str,
        /// The affected path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A descriptor-relative object-store maintenance operation failed.
    #[error("object-store maintenance failed: {0}")]
    ObjectMaintenance(#[source] rewind_platform::FileSystemError),
    /// A SQLite operation outside a migration failed.
    #[error("database operation failed while trying to {operation}: {source}")]
    Database {
        /// The attempted database operation.
        operation: &'static str,
        /// The SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// A numbered migration failed and was rolled back.
    #[error("schema migration {version} failed; the transaction was rolled back: {source}")]
    Migration {
        /// The failed target schema version.
        version: u32,
        /// The SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// The database was created by a newer Rewind version.
    #[error("store schema version {found} is newer than supported version {supported}")]
    NewerSchema {
        /// The version in the database.
        found: u32,
        /// The newest understood version.
        supported: u32,
    },
    /// Independent schema-version records disagree.
    #[error("store schema metadata is inconsistent (PRAGMA={pragma}, metadata={metadata})")]
    SchemaMismatch {
        /// SQLite's `user_version` value.
        pragma: u32,
        /// The explicit metadata-table value.
        metadata: u32,
    },
    /// Another writer owns the store lock.
    #[error("store writer lock {path} is held ({owner:?}); wait for the active writer to finish")]
    Locked {
        /// The lock path.
        path: PathBuf,
        /// Best-effort ownership diagnostics.
        owner: LockInfo,
    },
    /// The lock file exceeds its bound or cannot be decoded.
    #[error("store writer lock {path} is malformed: {message}")]
    MalformedLock {
        /// The malformed lock path.
        path: PathBuf,
        /// The validation failure.
        message: String,
    },
    /// The system clock cannot produce a Unix timestamp.
    #[error("system clock is before the Unix epoch: {source}")]
    Clock {
        /// The clock conversion error.
        #[source]
        source: SystemTimeError,
    },
    /// Imported or read bytes do not match their claimed identity.
    #[error("object digest mismatch: expected {expected}, computed {actual}")]
    ObjectDigestMismatch {
        /// The claimed digest.
        expected: ObjectId,
        /// The computed digest.
        actual: ObjectId,
    },
    /// An existing digest path contains different logical bytes.
    #[error("existing object {id} does not contain the claimed logical bytes")]
    ObjectCollision {
        /// The conflicting identity.
        id: ObjectId,
    },
    /// A durable snapshot or event still references the object.
    #[error("object {id} is still referenced")]
    ObjectReferenced {
        /// The reachable identity.
        id: ObjectId,
    },
    /// An object envelope violates its framing rules.
    #[error("malformed object envelope: {0}")]
    MalformedObjectEnvelope(&'static str),
    /// An object uses a future envelope version.
    #[error("object envelope version {0} is unsupported")]
    UnsupportedObjectEnvelopeVersion(u8),
    /// An object uses an unknown compression code.
    #[error("object compression method {0} is unsupported")]
    UnsupportedObjectCompression(u8),
    /// An object or aggregate cannot fit supported integer ranges.
    #[error("object is too large for this platform")]
    ObjectTooLarge,
    /// A caller refused to materialize an object larger than its memory budget.
    #[error("object is {actual} bytes; the in-memory read limit is {maximum} bytes")]
    ObjectReadLimit {
        /// Logical bytes declared by the validated envelope.
        actual: u64,
        /// Maximum logical bytes accepted by the caller.
        maximum: u64,
    },
    /// A mutation was attempted through a read-only handle.
    #[error("store is open read-only; {operation} requires the writer lock")]
    ReadOnly {
        /// The rejected mutation.
        operation: &'static str,
    },
    /// A requested durable record does not exist.
    #[error("{entity} {id} was not found")]
    NotFound {
        /// The record kind.
        entity: &'static str,
        /// The requested identity.
        id: String,
    },
    /// Stored identifier text is noncanonical.
    #[error("invalid durable identifier for {kind}: {value}")]
    InvalidIdentifier {
        /// The identifier kind.
        kind: &'static str,
        /// The rejected text.
        value: String,
    },
    /// Stored enum text is unknown.
    #[error("invalid stored {kind} value: {value}")]
    InvalidEnum {
        /// The enum kind.
        kind: &'static str,
        /// The rejected spelling.
        value: String,
    },
    /// A typed durable value could not be serialized.
    #[error("cannot serialize {context}: {source}")]
    Serialization {
        /// The value being serialized.
        context: &'static str,
        /// The serializer error.
        #[source]
        source: serde_json::Error,
    },
    /// Stored typed data could not be decoded.
    #[error("cannot deserialize {context}: {source}")]
    Deserialization {
        /// The value being decoded.
        context: &'static str,
        /// The decoder error.
        #[source]
        source: serde_json::Error,
    },
    /// An event batch is not contiguous or monotonic.
    #[error("event batch violates ordering: {message}")]
    EventOrder {
        /// The ordering violation.
        message: String,
    },
    /// An event violates a durable invariant.
    #[error("event {id} is invalid: {message}")]
    InvalidEvent {
        /// The event identity.
        id: String,
        /// The validation failure.
        message: String,
    },
    /// A page request is zero or exceeds the resource bound.
    #[error("timeline page limit {requested} is outside 1..={maximum}")]
    InvalidPageLimit {
        /// The caller's limit.
        requested: u32,
        /// The enforced maximum.
        maximum: u32,
    },
    /// A run violates lifecycle or cross-field invariants.
    #[error("run {id} is invalid: {message}")]
    InvalidRun {
        /// The run identity.
        id: String,
        /// The validation failure.
        message: String,
    },
    /// A snapshot is malformed or its digest disagrees.
    #[error("snapshot {id} is invalid: {message}")]
    InvalidSnapshot {
        /// The snapshot identity.
        id: String,
        /// The validation failure.
        message: String,
    },
    /// A checkpoint is inconsistent with its run or snapshot.
    #[error("checkpoint {id} is invalid: {message}")]
    InvalidCheckpoint {
        /// The checkpoint identity.
        id: String,
        /// The validation failure.
        message: String,
    },
    /// A proposed ancestry edge closes a cycle.
    #[error("run parent relationship would create a cycle: child {child}, parent {parent}")]
    ParentCycle {
        /// The proposed child.
        child: String,
        /// The proposed parent.
        parent: String,
    },
    /// A value cannot fit SQLite's signed integer representation.
    #[error("numeric value {value} for {field} cannot be stored safely")]
    NumericRange {
        /// The bounded field.
        field: &'static str,
        /// The rejected value.
        value: u128,
    },
    /// Stored data violates an invariant that public APIs preserve.
    #[error("internal store invariant failed: {message}")]
    Invariant {
        /// The violated invariant.
        message: String,
    },
}

/// Store operation result.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Decodes and validates one untrusted versioned object envelope.
///
/// The caller must still compare the returned logical bytes with the digest
/// supplied by the import manifest before installing them.
pub fn decode_object_envelope(envelope: &[u8]) -> Result<Vec<u8>> {
    object::decode(envelope)
}

/// SQLite metadata and its corresponding immutable object directory.
///
/// `open` owns the single store writer lock. `open_read_only` does not mutate
/// storage and remains usable while a writer records through SQLite WAL.
pub struct Store {
    root: PathBuf,
    connection: Connection,
    objects: ObjectStore,
    _lock: Option<lock::StoreLock>,
}

impl Store {
    /// Opens or initializes a writable store and acquires its writer lock.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let now = unix_millis(SystemTime::now())?;
        Self::open_at(root.as_ref(), now)
    }

    fn open_at(root: &Path, now_unix_ms: i64) -> Result<Self> {
        create_private_dir_all(root, "create store directory")?;
        let writer_lock = lock::StoreLock::acquire(root)?;
        let objects = ObjectStore::create(root.join("objects"))?;
        let database_path = root.join("metadata.sqlite3");
        let database_existed = database_path
            .try_exists()
            .map_err(|source| StoreError::Io {
                operation: "inspect metadata database",
                path: database_path.clone(),
                source,
            })?;
        let mut connection =
            Connection::open(&database_path).map_err(|source| StoreError::Database {
                operation: "open metadata database",
                source,
            })?;
        if !database_existed {
            restrict_file(&database_path)?;
        }
        configure_writer(&connection)?;
        schema::migrate(&mut connection, now_unix_ms)?;
        let mut store = Self {
            root: root.to_path_buf(),
            connection,
            objects,
            _lock: Some(writer_lock),
        };
        store.mark_interrupted_runs(Timestamp::from_unix_milliseconds(now_unix_ms))?;
        Ok(store)
    }

    /// Opens an initialized store without acquiring its writer lock.
    pub fn open_read_only(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let database_path = root.join("metadata.sqlite3");
        let connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| StoreError::Database {
            operation: "open metadata database read-only",
            source,
        })?;
        configure_reader(&connection)?;
        schema::verify(&connection)?;
        Ok(Self {
            root: root.to_path_buf(),
            connection,
            objects: ObjectStore::existing(root.join("objects")),
            _lock: None,
        })
    }

    /// Returns the resolved store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Borrows the immutable object store.
    pub fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// Reports whether this handle lacks the writer lock.
    pub fn is_read_only(&self) -> bool {
        self._lock.is_none()
    }

    /// Returns writer-lock diagnostics without changing the lock.
    pub fn inspect_writer_lock(root: impl AsRef<Path>) -> Result<Option<LockInfo>> {
        let path = root.as_ref().join("writer.lock");
        lock::inspect_held(&path)
    }

    /// Durably installs bytes and records their storage metadata.
    pub fn put_object(&mut self, bytes: &[u8], created_unix_ms: i64) -> Result<StoredObject> {
        self.require_writer("store an object")?;
        let stored = self.objects.put(bytes)?;
        self.record_object(stored, created_unix_ms)?;
        Ok(stored)
    }

    /// Imports bytes under a caller-provided digest after verifying it.
    pub fn import_object(
        &mut self,
        expected: ObjectId,
        bytes: &[u8],
        created_unix_ms: i64,
    ) -> Result<StoredObject> {
        self.require_writer("import an object")?;
        let stored = self.objects.put_verified(expected, bytes)?;
        self.record_object(stored, created_unix_ms)?;
        Ok(stored)
    }

    /// Loads logical object bytes within an explicit memory budget after
    /// envelope and digest verification.
    pub fn load_object(&self, id: ObjectId, maximum: u64) -> Result<Vec<u8>> {
        self.objects.get(id, maximum)
    }

    /// Opens a bounded streaming reader whose EOF verifies the object digest.
    pub fn open_object_reader(&self, id: ObjectId, maximum: u64) -> Result<ObjectReader> {
        self.objects.open_reader(id, maximum)
    }

    /// Checks indexed object metadata without reading object bytes.
    pub fn object_exists(&self, id: ObjectId) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT 1 FROM objects WHERE id = ?1",
                [id.to_string()],
                |_| Ok(()),
            )
            .optional()
            .map(|value| value.is_some())
            .map_err(|source| StoreError::Database {
                operation: "check object metadata",
                source,
            })
    }

    fn record_object(&mut self, stored: StoredObject, created_unix_ms: i64) -> Result<()> {
        let logical_size = sql_u64("object logical size", stored.logical_size)?;
        let stored_size = sql_u64("object stored size", stored.stored_size)?;
        self.connection
            .execute(
                "INSERT INTO objects(id, logical_size, stored_size, compression, created_unix_ms)\n                 VALUES (?1, ?2, ?3, ?4, ?5)\n                 ON CONFLICT(id) DO NOTHING",
                params![
                    stored.id.to_string(),
                    logical_size,
                    stored_size,
                    stored.compression.as_str(),
                    created_unix_ms
                ],
            )
            .map_err(|source| StoreError::Database {
                operation: "record object metadata",
                source,
            })?;

        let recorded: (i64, i64, String) = self
            .connection
            .query_row(
                "SELECT logical_size, stored_size, compression FROM objects WHERE id = ?1",
                [stored.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|source| StoreError::Database {
                operation: "verify object metadata",
                source,
            })?;
        if recorded
            != (
                logical_size,
                stored_size,
                stored.compression.as_str().to_owned(),
            )
        {
            return Err(StoreError::Invariant {
                message: format!("object metadata disagrees for {}", stored.id),
            });
        }
        Ok(())
    }

    fn require_writer(&self, operation: &'static str) -> Result<()> {
        if self._lock.is_some() {
            Ok(())
        } else {
            Err(StoreError::ReadOnly { operation })
        }
    }
}

fn configure_writer(connection: &Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .and_then(|()| connection.pragma_update(None, "foreign_keys", "ON"))
        .and_then(|()| connection.pragma_update(None, "journal_mode", "WAL"))
        .and_then(|()| connection.pragma_update(None, "synchronous", "FULL"))
        .map_err(|source| StoreError::Database {
            operation: "configure writable database",
            source,
        })
}

fn configure_reader(connection: &Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .and_then(|()| connection.pragma_update(None, "foreign_keys", "ON"))
        .and_then(|()| connection.pragma_update(None, "query_only", "ON"))
        .map_err(|source| StoreError::Database {
            operation: "configure read-only database",
            source,
        })
}

fn unix_millis(time: SystemTime) -> Result<i64> {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .map_err(|source| StoreError::Clock { source })?
        .as_millis();
    i64::try_from(millis).map_err(|_| StoreError::NumericRange {
        field: "Unix timestamp milliseconds",
        value: millis,
    })
}

fn sql_u64(field: &'static str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError::NumericRange {
        field,
        value: u128::from(value),
    })
}

pub(crate) fn create_private_dir_all(path: &Path, operation: &'static str) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })
}

fn restrict_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            StoreError::Io {
                operation: "restrict metadata database permissions",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}
pub use bundle::{
    BundleDecodeLimits, BundleEntry, BundleError, BundleStreamWriter, decode_bundle, encode_bundle,
    validate_archive_path,
};
