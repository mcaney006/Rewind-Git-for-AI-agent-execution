use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::{ObjectId, SnapshotId};

/// The current durable snapshot manifest schema.
pub const SNAPSHOT_SCHEMA_VERSION: u16 = 1;

/// Maximum UTF-8 byte length of a normalized snapshot path.
pub const MAX_SNAPSHOT_PATH_BYTES: usize = 4096;

/// A normalized, nonempty, relative UTF-8 path stored with `/` separators.
///
/// Absolute paths, empty components, `.`, `..`, backslashes, and NUL bytes are
/// rejected before the path can enter a manifest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotPath(String);

impl SnapshotPath {
    /// Borrows the canonical slash-separated representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the path and returns its canonical representation.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for SnapshotPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SnapshotPath {
    type Err = SnapshotPathError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Err(SnapshotPathError::Empty);
        }
        if input.len() > MAX_SNAPSHOT_PATH_BYTES {
            return Err(SnapshotPathError::TooLong {
                actual: input.len(),
                maximum: MAX_SNAPSHOT_PATH_BYTES,
            });
        }
        if input.starts_with('/') {
            return Err(SnapshotPathError::Absolute);
        }
        if input.as_bytes().contains(&0) {
            return Err(SnapshotPathError::Nul);
        }
        if input.contains('\\') {
            return Err(SnapshotPathError::Backslash);
        }
        for component in input.split('/') {
            match component {
                "" => return Err(SnapshotPathError::EmptyComponent),
                "." => return Err(SnapshotPathError::CurrentDirectory),
                ".." => return Err(SnapshotPathError::ParentDirectory),
                _ => {}
            }
        }
        Ok(Self(input.to_owned()))
    }
}

impl TryFrom<String> for SnapshotPath {
    type Error = SnapshotPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl Serialize for SnapshotPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SnapshotPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PathVisitor;

        impl Visitor<'_> for PathVisitor {
            type Value = SnapshotPath;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a normalized nonempty relative UTF-8 path")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(PathVisitor)
    }
}

/// A rejected snapshot path.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SnapshotPathError {
    /// The root itself is represented by the manifest and is not an entry.
    #[error("snapshot paths must not be empty")]
    Empty,
    /// Resource bounds prohibit arbitrarily long durable paths.
    #[error("snapshot path is {actual} bytes; the maximum is {maximum}")]
    TooLong {
        /// The actual UTF-8 byte length.
        actual: usize,
        /// The configured domain maximum.
        maximum: usize,
    },
    /// Snapshot entries are always relative to their materialization root.
    #[error("absolute snapshot paths are forbidden")]
    Absolute,
    /// NUL cannot be represented by supported filesystem APIs.
    #[error("snapshot paths must not contain NUL")]
    Nul,
    /// Backslash is rejected to avoid platform-dependent interpretation.
    #[error("snapshot paths must use forward slashes, not backslashes")]
    Backslash,
    /// Repeated or trailing separators are not canonical.
    #[error("snapshot paths must not contain empty components")]
    EmptyComponent,
    /// Current-directory components are not canonical.
    #[error("snapshot paths must not contain '.' components")]
    CurrentDirectory,
    /// Parent traversal could escape a materialization root.
    #[error("snapshot paths must not contain '..' components")]
    ParentDirectory,
}

/// Supported Unix permission bits (`0o0000..=0o7777`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct UnixPermissions(u16);

impl UnixPermissions {
    /// The largest supported permission and special-bit mask.
    pub const MASK: u16 = 0o7777;

    /// Constructs permissions after rejecting unsupported bits.
    pub const fn new(bits: u16) -> Result<Self, InvalidUnixPermissions> {
        if bits & !Self::MASK == 0 {
            Ok(Self(bits))
        } else {
            Err(InvalidUnixPermissions { bits })
        }
    }

    /// Returns the preserved permission bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Returns whether any owner, group, or other execute bit is set.
    #[must_use]
    pub const fn is_executable(self) -> bool {
        self.0 & 0o111 != 0
    }
}

impl TryFrom<u16> for UnixPermissions {
    type Error = InvalidUnixPermissions;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<UnixPermissions> for u16 {
    fn from(value: UnixPermissions) -> Self {
        value.bits()
    }
}

/// Permission bits outside the supported Unix mask.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unsupported Unix permission bits {bits:#o}")]
pub struct InvalidUnixPermissions {
    /// The rejected bits.
    pub bits: u16,
}

/// The durable kind-specific data for a workspace entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotEntryKind {
    /// A directory whose children appear as separate manifest entries.
    Directory,
    /// A regular file backed by immutable content bytes.
    File {
        /// The BLAKE3 identity of the file contents.
        object_id: ObjectId,
        /// The exact uncompressed file length.
        size: u64,
        /// Whether at least one Unix execute bit was present.
        executable: bool,
    },
    /// A symbolic link, recorded without following it.
    Symlink {
        /// The link target exactly as representable in UTF-8.
        target: String,
    },
}

/// One normalized path and its supported filesystem state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    /// The normalized path relative to the snapshot root.
    pub path: SnapshotPath,
    /// Kind-specific persisted data.
    pub kind: SnapshotEntryKind,
    /// Supported Unix mode bits captured without file-type bits.
    pub permissions: UnixPermissions,
}

impl SnapshotEntry {
    /// Checks relationships between kind-specific data and common metadata.
    pub fn validate(&self) -> Result<(), SnapshotEntryError> {
        match &self.kind {
            SnapshotEntryKind::File { executable, .. }
                if *executable != self.permissions.is_executable() =>
            {
                Err(SnapshotEntryError::ExecutableBitMismatch)
            }
            SnapshotEntryKind::Symlink { target } if target.is_empty() => {
                Err(SnapshotEntryError::EmptySymlinkTarget)
            }
            SnapshotEntryKind::Symlink { target } if target.as_bytes().contains(&0) => {
                Err(SnapshotEntryError::NulSymlinkTarget)
            }
            SnapshotEntryKind::Symlink { target } if target.len() > MAX_SNAPSHOT_PATH_BYTES => {
                Err(SnapshotEntryError::SymlinkTargetTooLong)
            }
            SnapshotEntryKind::Directory
            | SnapshotEntryKind::File { .. }
            | SnapshotEntryKind::Symlink { .. } => Ok(()),
        }
    }
}

/// A violated invariant within one snapshot entry.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SnapshotEntryError {
    /// The explicit executable flag must agree with the preserved mode bits.
    #[error("file executable flag does not match Unix permission bits")]
    ExecutableBitMismatch,
    /// Supported filesystems do not permit empty symbolic-link targets.
    #[error("symbolic-link target must not be empty")]
    EmptySymlinkTarget,
    /// NUL cannot be represented by supported symbolic-link APIs.
    #[error("symbolic-link target must not contain NUL")]
    NulSymlinkTarget,
    /// Resource bounds prohibit arbitrarily long symbolic-link targets.
    #[error("symbolic-link target exceeds the supported length")]
    SymlinkTargetTooLong,
}

/// A canonical, deterministically ordered snapshot manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    /// The durable manifest schema version.
    pub schema_version: u16,
    entries: Vec<SnapshotEntry>,
}

impl SnapshotManifest {
    /// Builds a canonical manifest by validating and sorting entries by path.
    pub fn new(mut entries: Vec<SnapshotEntry>) -> Result<Self, SnapshotManifestError> {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Self::from_canonical_entries(SNAPSHOT_SCHEMA_VERSION, entries)
    }

    /// Validates entries that are already in their persisted canonical order.
    pub fn from_canonical_entries(
        schema_version: u16,
        entries: Vec<SnapshotEntry>,
    ) -> Result<Self, SnapshotManifestError> {
        if schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(SnapshotManifestError::UnsupportedSchemaVersion {
                found: schema_version,
                supported: SNAPSHOT_SCHEMA_VERSION,
            });
        }

        for (index, entry) in entries.iter().enumerate() {
            entry
                .validate()
                .map_err(|source| SnapshotManifestError::InvalidEntry {
                    path: entry.path.clone(),
                    source,
                })?;
            if let Some(previous) = index.checked_sub(1).and_then(|value| entries.get(value)) {
                match previous.path.cmp(&entry.path) {
                    std::cmp::Ordering::Equal => {
                        return Err(SnapshotManifestError::DuplicatePath {
                            path: entry.path.clone(),
                        });
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(SnapshotManifestError::NonCanonicalOrder { index });
                    }
                    std::cmp::Ordering::Less => {}
                }
            }
        }

        Ok(Self {
            schema_version,
            entries,
        })
    }

    /// Borrows canonically ordered entries.
    #[must_use]
    pub fn entries(&self) -> &[SnapshotEntry] {
        &self.entries
    }

    /// Consumes the manifest and returns canonically ordered entries.
    #[must_use]
    pub fn into_entries(self) -> Vec<SnapshotEntry> {
        self.entries
    }
}

impl<'de> Deserialize<'de> for SnapshotManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ManifestWire {
            schema_version: u16,
            entries: Vec<SnapshotEntry>,
        }

        let wire = ManifestWire::deserialize(deserializer)?;
        Self::from_canonical_entries(wire.schema_version, wire.entries).map_err(de::Error::custom)
    }
}

/// A rejected snapshot manifest.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SnapshotManifestError {
    /// Only explicitly understood schemas may be materialized.
    #[error("unsupported snapshot schema version {found}; supported version is {supported}")]
    UnsupportedSchemaVersion {
        /// The manifest's version.
        found: u16,
        /// The sole version understood by this crate.
        supported: u16,
    },
    /// A kind-specific entry invariant was violated.
    #[error("invalid snapshot entry {path}: {source}")]
    InvalidEntry {
        /// The invalid entry path.
        path: SnapshotPath,
        /// The violated entry invariant.
        source: SnapshotEntryError,
    },
    /// Every path may occur at most once.
    #[error("duplicate snapshot path {path}")]
    DuplicatePath {
        /// The duplicated path.
        path: SnapshotPath,
    },
    /// Persisted entries must already be in ascending canonical path order.
    #[error("snapshot entries are not in canonical order at index {index}")]
    NonCanonicalOrder {
        /// The first out-of-order entry index.
        index: usize,
    },
}

/// A snapshot identifier paired with its canonical manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// The BLAKE3 identity of the canonical manifest bytes.
    pub id: SnapshotId,
    /// The canonical workspace manifest.
    pub manifest: SnapshotManifest,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, byte: u8, executable: bool) -> SnapshotEntry {
        let permissions = UnixPermissions::new(if executable { 0o755 } else { 0o644 }).unwrap();
        SnapshotEntry {
            path: path.parse().unwrap(),
            kind: SnapshotEntryKind::File {
                object_id: ObjectId::digest(&[byte]),
                size: 1,
                executable,
            },
            permissions,
        }
    }

    #[test]
    fn path_validation_rejects_escape_and_ambiguous_forms() {
        for malicious in [
            "",
            "/absolute",
            "../escape",
            "a/../escape",
            "./a",
            "a/./b",
            "a//b",
            "a/",
            "a\\b",
            "a\0b",
        ] {
            assert!(
                malicious.parse::<SnapshotPath>().is_err(),
                "accepted {malicious:?}"
            );
        }
        for valid in ["a", ".git/config", "fixtures/a b", "é/文件"] {
            assert_eq!(valid.parse::<SnapshotPath>().unwrap().as_str(), valid);
        }
    }

    #[test]
    fn construction_canonicalizes_every_input_order() {
        let expected = SnapshotManifest::new(vec![
            file("a", 1, false),
            file("b", 2, false),
            file("c", 3, true),
        ])
        .unwrap();
        let expected_json = serde_json::to_vec(&expected).unwrap();
        let permutations = [
            vec![file("a", 1, false), file("b", 2, false), file("c", 3, true)],
            vec![file("c", 3, true), file("b", 2, false), file("a", 1, false)],
            vec![file("b", 2, false), file("a", 1, false), file("c", 3, true)],
            vec![file("c", 3, true), file("a", 1, false), file("b", 2, false)],
        ];
        for entries in permutations {
            let manifest = SnapshotManifest::new(entries).unwrap();
            assert_eq!(manifest, expected);
            assert_eq!(serde_json::to_vec(&manifest).unwrap(), expected_json);
        }
    }

    #[test]
    fn persisted_manifests_must_already_be_canonical() {
        let unordered = serde_json::json!({
            "schema_version": SNAPSHOT_SCHEMA_VERSION,
            "entries": [file("b", 2, false), file("a", 1, false)]
        });
        assert!(serde_json::from_value::<SnapshotManifest>(unordered).is_err());

        let duplicate = SnapshotManifest::new(vec![file("a", 1, false), file("a", 2, false)]);
        assert!(matches!(
            duplicate,
            Err(SnapshotManifestError::DuplicatePath { .. })
        ));
    }

    #[test]
    fn file_executable_flag_must_match_permissions() {
        let mut entry = file("script", 1, true);
        entry.permissions = UnixPermissions::new(0o644).unwrap();
        assert_eq!(
            entry.validate(),
            Err(SnapshotEntryError::ExecutableBitMismatch)
        );
        assert!(serde_json::from_str::<UnixPermissions>("65535").is_err());
    }
}
