use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use rewind_domain::{ObjectId, RunId, Timestamp};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::run::{decode_u64, parse_id};
use crate::{MAX_TIMELINE_PAGE, ObjectCompression, Result, Store, StoreError};

/// The maximum number of object records returned by one maintenance query.
pub const MAX_OBJECT_PAGE: u32 = MAX_TIMELINE_PAGE;

/// Indexed and measured sizes for one immutable object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectRecord {
    /// Logical content identity.
    pub id: ObjectId,
    /// Uncompressed logical byte length recorded at insertion.
    pub logical_size: u64,
    /// Envelope byte length recorded at insertion.
    pub stored_size: u64,
    /// Current physical file length, or `None` when the file is missing.
    pub physical_size: Option<u64>,
    /// Envelope compression method.
    pub compression: ObjectCompression,
    /// Wall-clock insertion time.
    pub created_at: Timestamp,
}

/// Bounded page of object metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectPage {
    /// Object records ordered by digest.
    pub objects: Vec<ObjectRecord>,
    /// Whether at least one later digest exists.
    pub has_more: bool,
}

/// Bounded page of referenced object identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectIdPage {
    /// Unique object identities ordered by digest.
    pub ids: Vec<ObjectId>,
    /// Whether at least one later identity exists.
    pub has_more: bool,
}

/// One corruption or metadata inconsistency found by bounded verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectCorruption {
    /// Affected object identity.
    pub id: ObjectId,
    /// Safe diagnostic without object contents.
    pub message: String,
}

/// An object omitted because the bounded verification sample exhausted its
/// logical-byte read budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectVerificationSkipped {
    /// Omitted object identity.
    pub id: ObjectId,
    /// Logical bytes recorded in metadata.
    pub logical_size: u64,
}

/// Results of a deterministic bounded object-store sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectVerification {
    /// Number of object paths actually checked.
    pub checked: u32,
    /// Corrupt or inconsistent objects found.
    pub corrupt: Vec<ObjectCorruption>,
    /// Sampled objects not read after the aggregate byte budget was exhausted.
    pub skipped: Vec<ObjectVerificationSkipped>,
}

/// Evidence returned after deleting one unreachable object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletedObject {
    /// Deleted identity.
    pub id: ObjectId,
    /// Recorded physical envelope bytes reclaimed.
    pub stored_size: u64,
}

/// A crash artifact present in the physical object store but absent from
/// indexed metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrphanFile {
    /// Canonical path below `objects/`, used as a stable page cursor.
    pub relative_path: String,
    /// Recognized artifact kind.
    pub kind: OrphanFileKind,
    /// Current physical file length.
    pub stored_size: u64,
}

/// Recognized unindexed physical artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrphanFileKind {
    /// A complete digest-shaped object published before its metadata row.
    Object(ObjectId),
    /// A private temporary object left before atomic publication.
    Temporary,
}

/// Bounded page of physical crash artifacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrphanFilePage {
    /// Entries ordered by canonical relative path.
    pub files: Vec<OrphanFile>,
    /// Whether at least one later recognized artifact exists.
    pub has_more: bool,
}

/// Evidence returned after deleting one physical crash artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletedOrphanFile {
    /// Removed path below `objects/`.
    pub relative_path: String,
    /// Removed artifact kind.
    pub kind: OrphanFileKind,
    /// Measured physical bytes reclaimed.
    pub stored_size: u64,
}

impl Store {
    /// Lists recognized physical object/temp files that have no SQLite object
    /// row. The scan is bounded in memory and never follows directory entries
    /// that are symlinks.
    pub fn list_physical_orphans(&self, after: Option<&str>, limit: u32) -> Result<OrphanFilePage> {
        self.require_writer("scan physical object crash artifacts")?;
        let capacity = page_capacity(limit)?;
        let retained = capacity.checked_add(1).ok_or(StoreError::ObjectTooLarge)?;
        let cursor = after.unwrap_or("");
        let mut files = BTreeMap::<String, OrphanFile>::new();
        // ponytail: this bounded page scan may revisit a crowded fan-out
        // directory; use a resumable native directory cursor only if GC
        // benchmarks justify the extra platform state.
        for prefix_value in 0_u16..=255 {
            let prefix = format!("{prefix_value:02x}");
            let directory = self.objects.root().join(&prefix);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => continue,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(StoreError::Io {
                        operation: "inspect object fan-out directory",
                        path: directory,
                        source,
                    });
                }
            }
            let entries = fs::read_dir(&directory).map_err(|source| StoreError::Io {
                operation: "enumerate object fan-out directory",
                path: directory.clone(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| StoreError::Io {
                    operation: "read object fan-out entry",
                    path: directory.clone(),
                    source,
                })?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let relative_path = format!("{prefix}/{name}");
                if relative_path.as_str() <= cursor {
                    continue;
                }
                let kind = if let Some(id) = digest_name(&prefix, &name) {
                    if self.object_exists(id)? {
                        continue;
                    }
                    OrphanFileKind::Object(id)
                } else if temporary_name(&name) {
                    OrphanFileKind::Temporary
                } else {
                    continue;
                };
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                        metadata
                    }
                    Ok(_) => continue,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(StoreError::Io {
                            operation: "inspect physical object artifact",
                            path,
                            source,
                        });
                    }
                };
                files.insert(
                    relative_path.clone(),
                    OrphanFile {
                        relative_path,
                        kind,
                        stored_size: metadata.len(),
                    },
                );
                if files.len() > retained {
                    files.pop_last();
                }
            }
        }
        let has_more = files.len() > capacity;
        let mut files = files.into_values().collect::<Vec<_>>();
        files.truncate(capacity);
        Ok(OrphanFilePage { files, has_more })
    }

    /// Deletes one previously listed crash artifact after rechecking that a
    /// digest-shaped object is still absent from indexed metadata.
    pub fn delete_physical_orphan(&mut self, orphan: &OrphanFile) -> Result<DeletedOrphanFile> {
        self.require_writer("delete physical object crash artifact")?;
        if let OrphanFileKind::Object(id) = orphan.kind {
            validate_event_object_references(&self.connection)?;
            if self.object_exists(id)? || object_is_referenced(&self.connection, id)? {
                return Err(StoreError::ObjectReferenced { id });
            }
        }
        validate_orphan(orphan)?;
        let root = rewind_platform::DirectoryRoot::open(self.objects.root())
            .map_err(StoreError::ObjectMaintenance)?;
        rewind_platform::remove_relative_entry(&root, &orphan.relative_path)
            .map_err(StoreError::ObjectMaintenance)?;
        Ok(DeletedOrphanFile {
            relative_path: orphan.relative_path.clone(),
            kind: orphan.kind,
            stored_size: orphan.stored_size,
        })
    }

    /// Lists indexed objects after a digest cursor, with measured physical sizes.
    pub fn list_objects(&self, after: Option<ObjectId>, limit: u32) -> Result<ObjectPage> {
        let capacity = page_capacity(limit)?;
        let cursor = after.map_or_else(String::new, |id| id.to_string());
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, logical_size, stored_size, compression, created_unix_ms\n                 FROM objects WHERE id > ?1 ORDER BY id LIMIT ?2",
            )
            .map_err(|source| StoreError::Database {
                operation: "prepare object listing",
                source,
            })?;
        let query_limit = i64::from(limit) + 1;
        let rows = statement
            .query_map(params![cursor, query_limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|source| StoreError::Database {
                operation: "query object listing",
                source,
            })?;
        let mut objects = Vec::with_capacity(capacity);
        for row in rows {
            let (raw_id, logical_size, stored_size, compression, created_unix_ms) =
                row.map_err(|source| StoreError::Database {
                    operation: "read object listing",
                    source,
                })?;
            let id = parse_id("object ID", &raw_id)?;
            let physical_size = match fs::metadata(self.objects.path_for(id)) {
                Ok(metadata) => Some(metadata.len()),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(StoreError::Io {
                        operation: "measure object file",
                        path: self.objects.path_for(id),
                        source,
                    });
                }
            };
            objects.push(ObjectRecord {
                id,
                logical_size: decode_u64("object logical size", logical_size)?,
                stored_size: decode_u64("object stored size", stored_size)?,
                physical_size,
                compression: parse_compression(&compression)?,
                created_at: Timestamp::from_unix_milliseconds(created_unix_ms),
            });
        }
        let has_more = objects.len() > capacity;
        objects.truncate(capacity);
        Ok(ObjectPage { objects, has_more })
    }

    /// Lists all object identities reachable from durable snapshot or event records.
    pub fn list_referenced_object_ids(
        &self,
        after: Option<ObjectId>,
        limit: u32,
    ) -> Result<ObjectIdPage> {
        let sql = "SELECT object_id FROM (\n                SELECT object_id FROM snapshot_entries WHERE object_id IS NOT NULL\n                UNION\n                SELECT object_id FROM terminal_chunks\n                UNION\n                SELECT CAST(json_extract(payload_json, '$.data.object_id') AS TEXT)\n                    FROM events\n                    WHERE json_extract(payload_json, '$.type') IN ('terminal_input', 'terminal_output')\n             ) WHERE object_id > ?1 ORDER BY object_id LIMIT ?2";
        self.query_object_ids(sql, after, limit, None)
    }

    /// Lists objects required to export or materialize one run's recorded history.
    pub fn list_run_object_ids(
        &self,
        run_id: RunId,
        after: Option<ObjectId>,
        limit: u32,
    ) -> Result<ObjectIdPage> {
        let sql = "SELECT object_id FROM (\n                SELECT snapshot_entries.object_id FROM snapshot_entries\n                    WHERE snapshot_entries.object_id IS NOT NULL\n                    AND snapshot_entries.snapshot_id IN (\n                        SELECT snapshot_id FROM checkpoints WHERE run_id = ?3\n                        UNION SELECT initial_snapshot_id FROM runs WHERE id = ?3 AND initial_snapshot_id IS NOT NULL\n                        UNION SELECT final_snapshot_id FROM runs WHERE id = ?3 AND final_snapshot_id IS NOT NULL\n                    )\n                UNION\n                SELECT CAST(json_extract(payload_json, '$.data.object_id') AS TEXT)\n                    FROM events WHERE run_id = ?3\n                    AND json_extract(payload_json, '$.type') IN ('terminal_input', 'terminal_output')\n             ) WHERE object_id > ?1 ORDER BY object_id LIMIT ?2";
        self.query_object_ids(sql, after, limit, Some(run_id))
    }

    /// Fails closed when event kinds or terminal-object references are malformed
    /// before garbage-collection reachability is trusted.
    pub fn validate_gc_references(&self) -> Result<()> {
        validate_event_object_references(&self.connection)
    }

    /// Verifies up to `limit` objects in digest order using constant memory and
    /// at most `maximum_logical_bytes` of declared payload reads.
    pub fn sample_object_corruption(
        &self,
        limit: u32,
        maximum_logical_bytes: u64,
    ) -> Result<ObjectVerification> {
        let page = self.list_objects(None, limit)?;
        let mut checked = 0_u32;
        let mut remaining = maximum_logical_bytes;
        let mut corrupt = Vec::new();
        let mut skipped = Vec::new();
        for record in page.objects {
            if record.logical_size > remaining {
                skipped.push(ObjectVerificationSkipped {
                    id: record.id,
                    logical_size: record.logical_size,
                });
                continue;
            }
            remaining -= record.logical_size;
            checked = checked.checked_add(1).ok_or(StoreError::NumericRange {
                field: "verified object count",
                value: u128::from(checked) + 1,
            })?;
            match self.objects.verify(record.id, record.logical_size) {
                Ok(measured)
                    if measured.logical_size == record.logical_size
                        && measured.stored_size == record.stored_size => {}
                Ok(measured) => corrupt.push(ObjectCorruption {
                    id: record.id,
                    message: format!(
                        "metadata sizes are logical/stored {}/{}, file contains {}/{}",
                        record.logical_size,
                        record.stored_size,
                        measured.logical_size,
                        measured.stored_size
                    ),
                }),
                Err(error) => corrupt.push(ObjectCorruption {
                    id: record.id,
                    message: error.to_string(),
                }),
            }
        }
        Ok(ObjectVerification {
            checked,
            corrupt,
            skipped,
        })
    }

    /// Deletes an object only after proving no durable record references it.
    pub fn delete_object(&mut self, id: ObjectId) -> Result<DeletedObject> {
        self.require_writer("delete an object")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin object deletion",
                source,
            })?;
        let stored_size: Option<i64> = transaction
            .query_row(
                "SELECT stored_size FROM objects WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| StoreError::Database {
                operation: "load object for deletion",
                source,
            })?;
        let stored_size = stored_size.ok_or_else(|| StoreError::NotFound {
            entity: "object",
            id: id.to_string(),
        })?;
        validate_event_object_references(&transaction)?;
        if object_is_referenced(&transaction, id)? {
            return Err(StoreError::ObjectReferenced { id });
        }
        let path = self.objects.path_for(id);
        if let Err(source) = fs::remove_file(&path)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(StoreError::Io {
                operation: "delete object file",
                path,
                source,
            });
        }
        transaction
            .execute("DELETE FROM objects WHERE id = ?1", [id.to_string()])
            .map_err(|source| StoreError::Database {
                operation: "delete object metadata",
                source,
            })?;
        transaction
            .commit()
            .map_err(|source| StoreError::Database {
                operation: "commit object deletion",
                source,
            })?;
        Ok(DeletedObject {
            id,
            stored_size: decode_u64("deleted object stored size", stored_size)?,
        })
    }

    fn query_object_ids(
        &self,
        sql: &str,
        after: Option<ObjectId>,
        limit: u32,
        run_id: Option<RunId>,
    ) -> Result<ObjectIdPage> {
        let capacity = page_capacity(limit)?;
        let cursor = after.map_or_else(String::new, |id| id.to_string());
        let query_limit = i64::from(limit) + 1;
        let mut statement =
            self.connection
                .prepare(sql)
                .map_err(|source| StoreError::Database {
                    operation: "prepare referenced object query",
                    source,
                })?;
        let mut rows = match run_id {
            Some(run_id) => statement.query(params![cursor, query_limit, run_id.to_string()]),
            None => statement.query(params![cursor, query_limit]),
        }
        .map_err(|source| StoreError::Database {
            operation: "query referenced objects",
            source,
        })?;
        let mut ids = Vec::with_capacity(capacity);
        while let Some(row) = rows.next().map_err(|source| StoreError::Database {
            operation: "read referenced objects",
            source,
        })? {
            let raw: String = row.get(0).map_err(|source| StoreError::Database {
                operation: "decode referenced object ID",
                source,
            })?;
            ids.push(parse_id("object ID", &raw)?);
        }
        let has_more = ids.len() > capacity;
        ids.truncate(capacity);
        Ok(ObjectIdPage { ids, has_more })
    }
}

fn digest_name(prefix: &str, name: &str) -> Option<ObjectId> {
    if name.len() != 62
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    format!("{prefix}{name}").parse().ok()
}

fn temporary_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".tmp-") else {
        return false;
    };
    let Some((pid, sequence)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !sequence.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

fn validate_orphan(orphan: &OrphanFile) -> Result<()> {
    let path = PathBuf::from(&orphan.relative_path);
    let mut components = path.components();
    let prefix = components
        .next()
        .and_then(|value| value.as_os_str().to_str());
    let name = components
        .next()
        .and_then(|value| value.as_os_str().to_str());
    if components.next().is_some() {
        return Err(StoreError::Invariant {
            message: format!("invalid physical orphan path {}", orphan.relative_path),
        });
    }
    let valid = match (prefix, name, orphan.kind) {
        (Some(prefix), Some(name), OrphanFileKind::Object(id)) => {
            digest_name(prefix, name) == Some(id)
        }
        (Some(prefix), Some(name), OrphanFileKind::Temporary) => {
            prefix.len() == 2
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && temporary_name(name)
        }
        _ => false,
    };
    if !valid {
        return Err(StoreError::Invariant {
            message: format!("invalid physical orphan path {}", orphan.relative_path),
        });
    }
    Ok(())
}

fn page_capacity(limit: u32) -> Result<usize> {
    if limit == 0 || limit > MAX_OBJECT_PAGE {
        return Err(StoreError::InvalidPageLimit {
            requested: limit,
            maximum: MAX_OBJECT_PAGE,
        });
    }
    usize::try_from(limit).map_err(|_| StoreError::NumericRange {
        field: "object page limit",
        value: u128::from(limit),
    })
}

fn parse_compression(value: &str) -> Result<ObjectCompression> {
    match value {
        "none" => Ok(ObjectCompression::None),
        _ => Err(StoreError::InvalidEnum {
            kind: "object compression",
            value: value.to_owned(),
        }),
    }
}

fn object_is_referenced(connection: &rusqlite::Connection, id: ObjectId) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(\n                SELECT 1 FROM snapshot_entries WHERE object_id = ?1\n                UNION ALL SELECT 1 FROM terminal_chunks WHERE object_id = ?1\n                UNION ALL SELECT 1 FROM events\n                    WHERE json_extract(payload_json, '$.type') IN ('terminal_input', 'terminal_output')\n                    AND CAST(json_extract(payload_json, '$.data.object_id') AS TEXT) = ?1\n             )",
            [id.to_string()],
            |row| row.get(0),
        )
        .map_err(|source| StoreError::Database {
            operation: "check object references",
            source,
        })
}

fn validate_event_object_references(connection: &rusqlite::Connection) -> Result<()> {
    let malformed: bool = connection
        .query_row(
            "SELECT EXISTS(\n                SELECT 1 FROM events\n                WHERE json_type(payload_json, '$.type') IS NOT 'text'\n                OR CAST(json_extract(payload_json, '$.type') AS TEXT) <> kind\n                OR (\n                    json_extract(payload_json, '$.type') IN ('terminal_input', 'terminal_output')\n                    AND (\n                        json_type(payload_json, '$.data.object_id') IS NOT 'text'\n                        OR length(json_extract(payload_json, '$.data.object_id')) <> 64\n                        OR json_extract(payload_json, '$.data.object_id') GLOB '*[^0-9a-f]*'\n                    )\n                )\n             )",
            [],
            |row| row.get(0),
        )
        .map_err(|source| StoreError::Database {
            operation: "validate event object references",
            source,
        })?;
    if malformed {
        Err(StoreError::Invariant {
            message: "malformed event kind or terminal object reference prevents safe deletion"
                .to_owned(),
        })
    } else {
        Ok(())
    }
}
