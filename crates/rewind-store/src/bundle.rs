use std::collections::BTreeSet;
use std::io::{Read, Write};

use thiserror::Error;

const MAGIC: &[u8; 4] = b"RWBN";
const BUNDLE_VERSION: u16 = 1;
const HEADER_BYTES: usize = 12;
const ENTRY_HEADER_BYTES: usize = 44;
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;

/// One named byte sequence in a Rewind bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleEntry {
    /// Normalized relative archive path.
    pub path: String,
    /// Complete entry payload.
    pub bytes: Vec<u8>,
}

/// Resource ceilings applied before the untrusted decoder allocates output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BundleDecodeLimits {
    /// Maximum number of entries accepted.
    pub maximum_entries: u32,
    /// Maximum payload size of one entry.
    pub maximum_entry_bytes: u64,
    /// Maximum aggregate payload bytes.
    pub maximum_total_bytes: u64,
}

impl Default for BundleDecodeLimits {
    fn default() -> Self {
        Self {
            maximum_entries: 1_000_000,
            maximum_entry_bytes: 16 * 1024 * 1024 * 1024,
            maximum_total_bytes: 64 * 1024 * 1024 * 1024,
        }
    }
}

/// A malformed, unsafe, corrupt, or unsupported bundle.
#[derive(Debug, Error)]
pub enum BundleError {
    /// Header magic does not identify a Rewind bundle.
    #[error("bundle magic is invalid")]
    InvalidMagic,
    /// The bundle uses a future format version.
    #[error("bundle version {found} is unsupported; supported version is {supported}")]
    UnsupportedVersion {
        /// Version read from the header.
        found: u16,
        /// Current decoder version.
        supported: u16,
    },
    /// Reserved header bits are nonzero.
    #[error("bundle contains unsupported flags")]
    UnsupportedFlags,
    /// The input ended before a declared field.
    #[error("bundle is truncated")]
    Truncated,
    /// Additional bytes remain after the declared final entry.
    #[error("bundle has trailing bytes")]
    TrailingBytes,
    /// An entry path is not valid UTF-8.
    #[error("bundle entry path is not valid UTF-8")]
    NonUtf8Path,
    /// An entry path could escape or has a noncanonical spelling.
    #[error("unsafe bundle entry path {0:?}")]
    UnsafePath(String),
    /// Paths must be strictly increasing and unique.
    #[error("bundle entry paths are not in strict canonical order at {0:?}")]
    NonCanonicalOrder(String),
    /// The number of entries exceeds the caller's ceiling.
    #[error("bundle declares {actual} entries; maximum is {maximum}")]
    TooManyEntries {
        /// Declared number of entries.
        actual: u32,
        /// Caller-provided ceiling.
        maximum: u32,
    },
    /// One entry exceeds the caller's byte ceiling.
    #[error("bundle entry {path:?} is {actual} bytes; maximum is {maximum}")]
    EntryTooLarge {
        /// Affected path.
        path: String,
        /// Declared length.
        actual: u64,
        /// Caller-provided ceiling.
        maximum: u64,
    },
    /// Aggregate entry payload exceeds the caller's ceiling.
    #[error("bundle payload exceeds the configured {maximum}-byte total")]
    TotalTooLarge {
        /// Caller-provided ceiling.
        maximum: u64,
    },
    /// An entry payload does not match its embedded BLAKE3 checksum.
    #[error("bundle entry checksum mismatch for {0:?}")]
    ChecksumMismatch(String),
    /// A count or length cannot be represented by the format or host.
    #[error("bundle size is outside the supported range")]
    NumericRange,
    /// The number of streamed entries differs from the declared header count.
    #[error("bundle declared {declared} entries but wrote {written}")]
    EntryCountMismatch {
        /// Header count.
        declared: u32,
        /// Entries actually supplied.
        written: u32,
    },
    /// Writing an encoded bundle failed.
    #[error("cannot write bundle bytes: {0}")]
    Io(#[from] std::io::Error),
    /// A streamed source ended early or contains more bytes than declared.
    #[error("bundle entry source length differs from its declaration for {0:?}")]
    SourceLengthMismatch(String),
}

/// Incremental deterministic bundle encoder with a declared entry count.
pub struct BundleStreamWriter<W> {
    writer: W,
    declared: u32,
    written: u32,
    previous: Option<String>,
}

impl<W: Write> BundleStreamWriter<W> {
    /// Writes a bundle header. Exactly `entry_count` ordered entries must follow.
    pub fn new(mut writer: W, entry_count: u32) -> Result<Self, BundleError> {
        writer.write_all(MAGIC)?;
        writer.write_all(&BUNDLE_VERSION.to_le_bytes())?;
        writer.write_all(&0_u16.to_le_bytes())?;
        writer.write_all(&entry_count.to_le_bytes())?;
        Ok(Self {
            writer,
            declared: entry_count,
            written: 0,
            previous: None,
        })
    }

    /// Writes one strictly increasing safe path and its checksummed payload.
    pub fn write_entry(&mut self, path: &str, bytes: &[u8]) -> Result<(), BundleError> {
        let byte_len = u64::try_from(bytes.len()).map_err(|_| BundleError::NumericRange)?;
        let checksum = *blake3::hash(bytes).as_bytes();
        self.write_entry_header(path, byte_len, &checksum)?;
        self.writer.write_all(bytes)?;
        self.complete_entry(path);
        Ok(())
    }

    /// Copies one prehashed entry from a reader with constant memory.
    ///
    /// The checksum is verified again while copying. Any mismatch invalidates
    /// the output stream, which callers should construct in a temporary file.
    pub fn write_prehashed_entry<R: Read>(
        &mut self,
        path: &str,
        byte_len: u64,
        checksum: [u8; 32],
        mut reader: R,
    ) -> Result<(), BundleError> {
        self.write_entry_header(path, byte_len, &checksum)?;
        let mut remaining = byte_len;
        let mut buffer = [0_u8; 64 * 1024];
        let mut hasher = blake3::Hasher::new();
        while remaining != 0 {
            let wanted = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| BundleError::NumericRange)?;
            let read = reader.read(&mut buffer[..wanted])?;
            if read == 0 {
                return Err(BundleError::SourceLengthMismatch(path.to_owned()));
            }
            hasher.update(&buffer[..read]);
            self.writer.write_all(&buffer[..read])?;
            remaining -= u64::try_from(read).map_err(|_| BundleError::NumericRange)?;
        }
        if reader.read(&mut buffer[..1])? != 0 {
            return Err(BundleError::SourceLengthMismatch(path.to_owned()));
        }
        if hasher.finalize().as_bytes() != &checksum {
            return Err(BundleError::ChecksumMismatch(path.to_owned()));
        }
        self.complete_entry(path);
        Ok(())
    }

    fn write_entry_header(
        &mut self,
        path: &str,
        byte_len: u64,
        checksum: &[u8; 32],
    ) -> Result<(), BundleError> {
        validate_archive_path(path)?;
        if self.written == self.declared {
            return Err(BundleError::EntryCountMismatch {
                declared: self.declared,
                written: self.written.saturating_add(1),
            });
        }
        if self
            .previous
            .as_deref()
            .is_some_and(|previous| previous >= path)
        {
            return Err(BundleError::NonCanonicalOrder(path.to_owned()));
        }
        let path_len = u16::try_from(path.len()).map_err(|_| BundleError::NumericRange)?;
        self.writer.write_all(&path_len.to_le_bytes())?;
        self.writer.write_all(&0_u16.to_le_bytes())?;
        self.writer.write_all(&byte_len.to_le_bytes())?;
        self.writer.write_all(checksum)?;
        self.writer.write_all(path.as_bytes())?;
        Ok(())
    }

    fn complete_entry(&mut self, path: &str) {
        self.previous = Some(path.to_owned());
        self.written += 1;
    }

    /// Verifies the count and returns the underlying writer.
    pub fn finish(self) -> Result<W, BundleError> {
        if self.written != self.declared {
            return Err(BundleError::EntryCountMismatch {
                declared: self.declared,
                written: self.written,
            });
        }
        Ok(self.writer)
    }
}

/// Encodes entries in deterministic path order with a checksum per payload.
pub fn encode_bundle(mut entries: Vec<BundleEntry>) -> Result<Vec<u8>, BundleError> {
    for entry in &entries {
        validate_archive_path(&entry.path)?;
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut paths = BTreeSet::new();
    for entry in &entries {
        if !paths.insert(entry.path.as_str()) {
            return Err(BundleError::NonCanonicalOrder(entry.path.clone()));
        }
    }
    let count = u32::try_from(entries.len()).map_err(|_| BundleError::NumericRange)?;
    let mut writer = BundleStreamWriter::new(Vec::new(), count)?;
    for entry in entries {
        writer.write_entry(&entry.path, &entry.bytes)?;
    }
    writer.finish()
}

/// Decodes an untrusted bundle only after checking framing, paths, bounds, and checksums.
pub fn decode_bundle(
    bytes: &[u8],
    limits: BundleDecodeLimits,
) -> Result<Vec<BundleEntry>, BundleError> {
    if bytes.len() < HEADER_BYTES {
        return Err(BundleError::Truncated);
    }
    if &bytes[..4] != MAGIC {
        return Err(BundleError::InvalidMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != BUNDLE_VERSION {
        return Err(BundleError::UnsupportedVersion {
            found: version,
            supported: BUNDLE_VERSION,
        });
    }
    if u16::from_le_bytes([bytes[6], bytes[7]]) != 0 {
        return Err(BundleError::UnsupportedFlags);
    }
    let count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if count > limits.maximum_entries {
        return Err(BundleError::TooManyEntries {
            actual: count,
            maximum: limits.maximum_entries,
        });
    }
    let capacity = usize::try_from(count).map_err(|_| BundleError::NumericRange)?;
    let mut entries = Vec::with_capacity(capacity);
    let mut cursor = HEADER_BYTES;
    let mut total = 0_u64;
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let header = take(bytes, &mut cursor, ENTRY_HEADER_BYTES)?;
        let path_len = usize::from(u16::from_le_bytes([header[0], header[1]]));
        if u16::from_le_bytes([header[2], header[3]]) != 0 {
            return Err(BundleError::UnsupportedFlags);
        }
        let byte_len = u64::from_le_bytes(
            header[4..12]
                .try_into()
                .map_err(|_| BundleError::Truncated)?,
        );
        let checksum: [u8; 32] = header[12..44]
            .try_into()
            .map_err(|_| BundleError::Truncated)?;
        let path_bytes = take(bytes, &mut cursor, path_len)?;
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| BundleError::NonUtf8Path)?
            .to_owned();
        validate_archive_path(&path)?;
        if previous.as_ref().is_some_and(|value| value >= &path) {
            return Err(BundleError::NonCanonicalOrder(path));
        }
        if byte_len > limits.maximum_entry_bytes {
            return Err(BundleError::EntryTooLarge {
                path,
                actual: byte_len,
                maximum: limits.maximum_entry_bytes,
            });
        }
        total = total
            .checked_add(byte_len)
            .ok_or(BundleError::TotalTooLarge {
                maximum: limits.maximum_total_bytes,
            })?;
        if total > limits.maximum_total_bytes {
            return Err(BundleError::TotalTooLarge {
                maximum: limits.maximum_total_bytes,
            });
        }
        let length = usize::try_from(byte_len).map_err(|_| BundleError::NumericRange)?;
        let payload = take(bytes, &mut cursor, length)?;
        if blake3::hash(payload).as_bytes() != &checksum {
            return Err(BundleError::ChecksumMismatch(path));
        }
        previous = Some(path.clone());
        entries.push(BundleEntry {
            path,
            bytes: payload.to_vec(),
        });
    }
    if cursor != bytes.len() {
        return Err(BundleError::TrailingBytes);
    }
    Ok(entries)
}

/// Rejects absolute, traversing, ambiguous, platform-dependent, and oversized paths.
pub fn validate_archive_path(path: &str) -> Result<(), BundleError> {
    if path.is_empty()
        || path.len() > MAX_ARCHIVE_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.as_bytes().contains(&0)
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(BundleError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], BundleError> {
    let end = cursor
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or(BundleError::Truncated)?;
    let slice = &bytes[*cursor..end];
    *cursor = end;
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_sorted_and_deterministic() {
        let entries = vec![
            BundleEntry {
                path: "z/data".to_owned(),
                bytes: vec![2],
            },
            BundleEntry {
                path: "a.json".to_owned(),
                bytes: vec![1],
            },
        ];
        let encoded = encode_bundle(entries).unwrap();
        let decoded = decode_bundle(&encoded, BundleDecodeLimits::default()).unwrap();
        assert_eq!(decoded[0].path, "a.json");
        assert_eq!(encode_bundle(decoded).unwrap(), encoded);
    }

    #[test]
    fn unsafe_paths_never_enter_an_archive() {
        for path in ["", "/etc/passwd", "../x", "a/../x", "a//x", "a\\x"] {
            assert!(validate_archive_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn corruption_and_resource_claims_fail_before_payload_allocation() {
        let mut encoded = encode_bundle(vec![BundleEntry {
            path: "data".to_owned(),
            bytes: vec![1, 2, 3],
        }])
        .unwrap();
        *encoded.last_mut().unwrap() ^= 1;
        assert!(matches!(
            decode_bundle(&encoded, BundleDecodeLimits::default()),
            Err(BundleError::ChecksumMismatch(_))
        ));

        let mut header = encoded[..HEADER_BYTES].to_vec();
        header[8..12].copy_from_slice(&2_u32.to_le_bytes());
        assert!(matches!(
            decode_bundle(
                &header,
                BundleDecodeLimits {
                    maximum_entries: 1,
                    ..BundleDecodeLimits::default()
                }
            ),
            Err(BundleError::TooManyEntries { .. })
        ));
    }
}
