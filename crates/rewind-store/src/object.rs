use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rewind_domain::ObjectId;

use crate::{Result, StoreError, create_private_dir_all};

const MAGIC: &[u8; 4] = b"RWOB";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 16;
const COMPRESSION_NONE: u8 = 0;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Immutable content-addressed storage rooted below a Rewind store.
#[derive(Debug, Clone)]
pub struct ObjectStore {
    root: PathBuf,
}

/// Metadata recorded after an object has been durably installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredObject {
    /// BLAKE3 identity of the logical bytes.
    pub id: ObjectId,
    /// Uncompressed byte length.
    pub logical_size: u64,
    /// Envelope byte length on disk.
    pub stored_size: u64,
    /// Envelope compression method.
    pub compression: ObjectCompression,
}

/// Bounded logical-object payload reader.
///
/// Reading through EOF verifies that the payload length and BLAKE3 identity
/// match the requested object. Callers must consume the reader completely.
#[derive(Debug)]
pub struct ObjectReader {
    file: File,
    path: PathBuf,
    id: ObjectId,
    logical_size: u64,
    remaining: u64,
    hasher: blake3::Hasher,
    verified: bool,
}

impl ObjectReader {
    /// Returns the validated logical length from the object envelope.
    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }
}

impl Read for ObjectReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if self.verified {
                return Ok(0);
            }
            let mut extra = [0_u8; 1];
            if self.file.read(&mut extra)? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("object {} grew while it was read", self.path.display()),
                ));
            }
            let actual = ObjectId::from_bytes(*self.hasher.finalize().as_bytes());
            if actual != self.id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "object digest mismatch: expected {}, computed {actual}",
                        self.id
                    ),
                ));
            }
            self.verified = true;
            return Ok(0);
        }
        let wanted = usize::try_from(self.remaining.min(buffer.len() as u64))
            .map_err(|_| io::Error::other("object read size exceeds this platform"))?;
        let count = self.file.read(&mut buffer[..wanted])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "object {} ended before its declared length",
                    self.path.display()
                ),
            ));
        }
        self.hasher.update(&buffer[..count]);
        self.remaining = self.remaining.saturating_sub(count as u64);
        Ok(count)
    }
}

/// Compression used by an object envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectCompression {
    /// Payload is stored verbatim.
    None,
}

impl ObjectCompression {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
        }
    }
}

impl ObjectStore {
    pub(crate) fn create(root: PathBuf) -> Result<Self> {
        create_private_dir_all(&root, "create object directory")?;
        Ok(Self { root })
    }

    pub(crate) fn existing(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the fan-out path for an object without reading it.
    pub fn path_for(&self, id: ObjectId) -> PathBuf {
        let digest = id.to_string();
        self.root.join(&digest[..2]).join(&digest[2..])
    }

    /// Stores logical bytes idempotently and returns their content identity.
    pub(crate) fn put(&self, bytes: &[u8]) -> Result<StoredObject> {
        let id = ObjectId::digest(bytes);
        self.put_verified(id, bytes)
    }

    /// Imports bytes only when they match the claimed content identity.
    pub(crate) fn put_verified(&self, expected: ObjectId, bytes: &[u8]) -> Result<StoredObject> {
        let actual = ObjectId::digest(bytes);
        if actual != expected {
            return Err(StoreError::ObjectDigestMismatch { expected, actual });
        }

        let final_path = self.path_for(expected);
        let parent = final_path.parent().ok_or_else(|| StoreError::Invariant {
            message: format!("object path has no parent: {}", final_path.display()),
        })?;
        create_private_dir_all(parent, "create object fan-out directory")?;

        if final_path.try_exists().map_err(|source| StoreError::Io {
            operation: "inspect object",
            path: final_path.clone(),
            source,
        })? {
            return self.verify_existing(expected, bytes);
        }

        let envelope = encode(bytes)?;
        let (temp_path, mut temp) = create_temp(parent)?;
        let install = (|| -> Result<()> {
            temp.write_all(&envelope).map_err(|source| StoreError::Io {
                operation: "write temporary object",
                path: temp_path.clone(),
                source,
            })?;
            temp.sync_all().map_err(|source| StoreError::Io {
                operation: "flush temporary object",
                path: temp_path.clone(),
                source,
            })?;
            drop(temp);

            match fs::hard_link(&temp_path, &final_path) {
                Ok(()) => {
                    sync_directory(parent)?;
                    Ok(())
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    self.verify_existing(expected, bytes).map(|_| ())
                }
                Err(source) => Err(StoreError::Io {
                    operation: "atomically install object",
                    path: final_path.clone(),
                    source,
                }),
            }
        })();
        let cleanup = fs::remove_file(&temp_path);
        if let Err(source) = cleanup
            && source.kind() != io::ErrorKind::NotFound
            && install.is_ok()
        {
            return Err(StoreError::Io {
                operation: "remove temporary object",
                path: temp_path,
                source,
            });
        }
        install?;

        Ok(StoredObject {
            id: expected,
            logical_size: u64::try_from(bytes.len()).map_err(|_| StoreError::ObjectTooLarge)?,
            stored_size: u64::try_from(envelope.len()).map_err(|_| StoreError::ObjectTooLarge)?,
            compression: ObjectCompression::None,
        })
    }

    /// Reads and verifies an object's envelope, length, and BLAKE3 identity.
    pub fn open_reader(&self, id: ObjectId, maximum: u64) -> Result<ObjectReader> {
        let path = self.path_for(id);
        let mut file = File::open(&path).map_err(|source| StoreError::Io {
            operation: "open object",
            path: path.clone(),
            source,
        })?;
        let stored_size = file
            .metadata()
            .map_err(|source| StoreError::Io {
                operation: "read object metadata",
                path: path.clone(),
                source,
            })?
            .len();
        let mut header = [0_u8; HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|source| StoreError::Io {
                operation: "read object envelope header",
                path: path.clone(),
                source,
            })?;
        let logical_size = decode_header(&header)?;
        let expected_stored_size = logical_size
            .checked_add(HEADER_LEN as u64)
            .ok_or(StoreError::ObjectTooLarge)?;
        if stored_size != expected_stored_size {
            return Err(StoreError::MalformedObjectEnvelope(
                "logical length does not match stored size",
            ));
        }
        if logical_size > maximum {
            return Err(StoreError::ObjectReadLimit {
                actual: logical_size,
                maximum,
            });
        }
        Ok(ObjectReader {
            file,
            path,
            id,
            logical_size,
            remaining: logical_size,
            hasher: blake3::Hasher::new(),
            verified: false,
        })
    }

    /// Reads an object into memory after enforcing the caller's byte ceiling.
    pub fn get(&self, id: ObjectId, maximum: u64) -> Result<Vec<u8>> {
        let mut reader = self.open_reader(id, maximum)?;
        let logical_size = reader.logical_size();
        let capacity = usize::try_from(logical_size).map_err(|_| StoreError::ObjectTooLarge)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| StoreError::ObjectTooLarge)?;
        bytes.resize(capacity, 0);
        reader
            .read_exact(&mut bytes)
            .map_err(|source| StoreError::Io {
                operation: "read object payload",
                path: reader.path.clone(),
                source,
            })?;
        let mut extra = [0_u8; 1];
        if reader.read(&mut extra).map_err(|source| StoreError::Io {
            operation: "verify object payload",
            path: reader.path.clone(),
            source,
        })? != 0
        {
            return Err(StoreError::Invariant {
                message: "bounded object reader returned bytes beyond its declared length"
                    .to_owned(),
            });
        }
        Ok(bytes)
    }

    /// Verifies an object within an explicit logical-byte read budget and
    /// returns its measured metadata.
    pub fn verify(&self, id: ObjectId, maximum: u64) -> Result<StoredObject> {
        let path = self.path_for(id);
        let mut file = File::open(&path).map_err(|source| StoreError::Io {
            operation: "open object for verification",
            path: path.clone(),
            source,
        })?;
        let stored_size = file
            .metadata()
            .map_err(|source| StoreError::Io {
                operation: "read object metadata for verification",
                path: path.clone(),
                source,
            })?
            .len();
        let mut header = [0_u8; HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|source| StoreError::Io {
                operation: "read object envelope header",
                path: path.clone(),
                source,
            })?;
        let logical_size = decode_header(&header)?;
        let expected_stored_size = logical_size
            .checked_add(HEADER_LEN as u64)
            .ok_or(StoreError::ObjectTooLarge)?;
        if stored_size != expected_stored_size {
            return Err(StoreError::MalformedObjectEnvelope(
                "logical length does not match stored size",
            ));
        }
        if logical_size > maximum {
            return Err(StoreError::ObjectReadLimit {
                actual: logical_size,
                maximum,
            });
        }
        let mut remaining = logical_size;
        let mut buffer = [0_u8; 64 * 1024];
        let mut hasher = blake3::Hasher::new();
        while remaining > 0 {
            let read_len = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| StoreError::ObjectTooLarge)?;
            file.read_exact(&mut buffer[..read_len])
                .map_err(|source| StoreError::Io {
                    operation: "read object payload for verification",
                    path: path.clone(),
                    source,
                })?;
            hasher.update(&buffer[..read_len]);
            remaining -= read_len as u64;
        }
        let actual = ObjectId::from_bytes(*hasher.finalize().as_bytes());
        if actual != id {
            return Err(StoreError::ObjectDigestMismatch {
                expected: id,
                actual,
            });
        }
        Ok(StoredObject {
            id,
            logical_size,
            stored_size,
            compression: ObjectCompression::None,
        })
    }

    fn verify_existing(&self, id: ObjectId, expected_bytes: &[u8]) -> Result<StoredObject> {
        let maximum =
            u64::try_from(expected_bytes.len()).map_err(|_| StoreError::ObjectTooLarge)?;
        let bytes = self.get(id, maximum)?;
        if bytes != expected_bytes {
            return Err(StoreError::ObjectCollision { id });
        }
        let stored_size = fs::metadata(self.path_for(id))
            .map_err(|source| StoreError::Io {
                operation: "read existing object metadata",
                path: self.path_for(id),
                source,
            })?
            .len();
        Ok(StoredObject {
            id,
            logical_size: u64::try_from(bytes.len()).map_err(|_| StoreError::ObjectTooLarge)?,
            stored_size,
            compression: ObjectCompression::None,
        })
    }
}

pub(crate) fn decode(envelope: &[u8]) -> Result<Vec<u8>> {
    if envelope.len() < HEADER_LEN || &envelope[..4] != MAGIC {
        return Err(StoreError::MalformedObjectEnvelope(
            "invalid magic or truncated header",
        ));
    }
    let logical_len = usize::try_from(decode_header(&envelope[..HEADER_LEN])?)
        .map_err(|_| StoreError::ObjectTooLarge)?;
    let payload = &envelope[HEADER_LEN..];
    if payload.len() != logical_len {
        return Err(StoreError::MalformedObjectEnvelope(
            "logical length does not match payload",
        ));
    }
    Ok(payload.to_vec())
}

fn decode_header(header: &[u8]) -> Result<u64> {
    if header.len() != HEADER_LEN || &header[..4] != MAGIC {
        return Err(StoreError::MalformedObjectEnvelope(
            "invalid magic or truncated header",
        ));
    }
    if header[4] != VERSION {
        return Err(StoreError::UnsupportedObjectEnvelopeVersion(header[4]));
    }
    if header[5] != COMPRESSION_NONE {
        return Err(StoreError::UnsupportedObjectCompression(header[5]));
    }
    if header[6..8] != [0, 0] {
        return Err(StoreError::MalformedObjectEnvelope(
            "reserved header bytes are nonzero",
        ));
    }
    let length_bytes: [u8; 8] = header[8..16]
        .try_into()
        .map_err(|_| StoreError::MalformedObjectEnvelope("missing logical length"))?;
    Ok(u64::from_le_bytes(length_bytes))
}

fn encode(bytes: &[u8]) -> Result<Vec<u8>> {
    let logical_len = u64::try_from(bytes.len()).map_err(|_| StoreError::ObjectTooLarge)?;
    let capacity = HEADER_LEN
        .checked_add(bytes.len())
        .ok_or(StoreError::ObjectTooLarge)?;
    let mut envelope = Vec::with_capacity(capacity);
    envelope.extend_from_slice(MAGIC);
    envelope.push(VERSION);
    envelope.push(COMPRESSION_NONE);
    envelope.extend_from_slice(&[0, 0]);
    envelope.extend_from_slice(&logical_len.to_le_bytes());
    envelope.extend_from_slice(bytes);
    Ok(envelope)
}

fn create_temp(parent: &Path) -> Result<(PathBuf, File)> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".tmp-{}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "create temporary object",
                    path,
                    source,
                });
            }
        }
    }
    Err(StoreError::Invariant {
        message: format!(
            "could not allocate a unique temporary file in {}",
            parent.display()
        ),
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| StoreError::Io {
            operation: "flush object directory",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_rejects_bad_length() {
        let mut envelope = encode(b"abc").unwrap();
        envelope[8] = 4;
        assert!(matches!(
            decode(&envelope),
            Err(StoreError::MalformedObjectEnvelope(_))
        ));
    }

    #[test]
    fn envelope_rejects_unknown_version_and_compression() {
        let mut envelope = encode(b"abc").unwrap();
        envelope[4] = 2;
        assert!(matches!(
            decode(&envelope),
            Err(StoreError::UnsupportedObjectEnvelopeVersion(2))
        ));

        let mut envelope = encode(b"abc").unwrap();
        envelope[5] = 1;
        assert!(matches!(
            decode(&envelope),
            Err(StoreError::UnsupportedObjectCompression(1))
        ));
    }

    #[test]
    fn object_read_checks_declared_size_before_allocating() {
        let temp = tempfile::tempdir().unwrap();
        let store = ObjectStore::create(temp.path().join("objects")).unwrap();
        let id = ObjectId::digest(b"sparse corrupt object");
        let path = store.path_for(id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let logical_size = 1_u64 << 40;
        let mut file = File::create(&path).unwrap();
        file.write_all(MAGIC).unwrap();
        file.write_all(&[VERSION, COMPRESSION_NONE, 0, 0]).unwrap();
        file.write_all(&logical_size.to_le_bytes()).unwrap();
        file.set_len(logical_size + HEADER_LEN as u64).unwrap();
        drop(file);

        assert!(matches!(
            store.get(id, 64),
            Err(StoreError::ObjectReadLimit {
                actual,
                maximum: 64
            }) if actual == logical_size
        ));
        assert!(matches!(
            store.verify(id, 64),
            Err(StoreError::ObjectReadLimit {
                actual,
                maximum: 64
            }) if actual == logical_size
        ));
    }
}
