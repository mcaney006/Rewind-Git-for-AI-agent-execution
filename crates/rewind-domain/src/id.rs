use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::{Uuid, Version};

/// Failure to parse a canonical version 7 record identifier.
#[derive(Debug, Error)]
pub enum RecordIdParseError {
    /// The input is not a UUID.
    #[error("invalid UUID: {0}")]
    InvalidUuid(#[source] uuid::Error),
    /// The input is a UUID, but not canonical lowercase hyphenated text.
    #[error("record IDs must use canonical lowercase hyphenated UUID text")]
    NonCanonical,
    /// The UUID is not version 7.
    #[error("record IDs must be UUID version 7 (found version {found})")]
    WrongVersion {
        /// The version nibble found in the input.
        found: usize,
    },
}

fn parse_record_id(input: &str) -> Result<Uuid, RecordIdParseError> {
    let parsed = Uuid::parse_str(input).map_err(RecordIdParseError::InvalidUuid)?;
    if parsed.hyphenated().to_string() != input {
        return Err(RecordIdParseError::NonCanonical);
    }
    if parsed.get_version() != Some(Version::SortRand) {
        return Err(RecordIdParseError::WrongVersion {
            found: parsed.get_version_num(),
        });
    }
    Ok(parsed)
}

macro_rules! record_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a locally ordered version 7 identifier.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Consumes this value and returns the underlying UUID.
            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = RecordIdParseError;

            fn from_str(input: &str) -> Result<Self, Self::Err> {
                parse_record_id(input).map(Self)
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = RecordIdParseError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                value.hyphenated().to_string().parse()
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct IdVisitor;

                impl Visitor<'_> for IdVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a canonical lowercase version 7 UUID")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        value.parse().map_err(E::custom)
                    }
                }

                deserializer.deserialize_str(IdVisitor)
            }
        }
    };
}

record_id!(RunId, "A locally generated, time-sortable run identifier.");
record_id!(
    BranchId,
    "A locally generated identifier shared by records on one execution branch."
);
record_id!(
    CheckpointId,
    "A locally generated, time-sortable checkpoint identifier."
);
record_id!(
    EventId,
    "A locally generated, time-sortable event identifier."
);
record_id!(
    TerminalStreamId,
    "A locally generated identifier for one recorded terminal byte stream."
);

/// Failure to parse a canonical BLAKE3 digest identifier.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DigestIdParseError {
    /// A BLAKE3 digest is exactly 32 bytes, encoded as 64 hexadecimal digits.
    #[error("digest IDs must contain exactly 64 hexadecimal digits (found {actual})")]
    InvalidLength {
        /// The byte length of the provided string.
        actual: usize,
    },
    /// Only lowercase hexadecimal digits are canonical.
    #[error("digest ID contains a non-lowercase-hex character at byte {index}")]
    InvalidCharacter {
        /// The zero-based byte offset of the invalid character.
        index: usize,
    },
}

fn parse_digest(input: &str) -> Result<[u8; 32], DigestIdParseError> {
    if input.len() != 64 {
        return Err(DigestIdParseError::InvalidLength {
            actual: input.len(),
        });
    }

    let mut bytes = [0_u8; 32];
    for (index, pair) in input.as_bytes().chunks_exact(2).enumerate() {
        let high =
            hex_nibble(pair[0]).ok_or(DigestIdParseError::InvalidCharacter { index: index * 2 })?;
        let low = hex_nibble(pair[1]).ok_or(DigestIdParseError::InvalidCharacter {
            index: index * 2 + 1,
        })?;
        bytes[index] = high << 4 | low;
    }
    Ok(bytes)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

macro_rules! digest_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 32]);

        impl $name {
            /// Computes an identifier from the exact supplied bytes.
            #[must_use]
            pub fn digest(bytes: &[u8]) -> Self {
                Self(*blake3::hash(bytes).as_bytes())
            }

            /// Constructs an identifier from an already verified digest.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// Borrows the 32 digest bytes.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Consumes this value and returns the 32 digest bytes.
            #[must_use]
            pub const fn into_bytes(self) -> [u8; 32] {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl FromStr for $name {
            type Err = DigestIdParseError;

            fn from_str(input: &str) -> Result<Self, Self::Err> {
                parse_digest(input).map(Self)
            }
        }

        impl From<[u8; 32]> for $name {
            fn from(value: [u8; 32]) -> Self {
                Self(value)
            }
        }

        impl From<$name> for [u8; 32] {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct DigestVisitor;

                impl Visitor<'_> for DigestVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a 64-character lowercase BLAKE3 digest")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        value.parse().map_err(E::custom)
                    }
                }

                deserializer.deserialize_str(DigestVisitor)
            }
        }
    };
}

digest_id!(
    SnapshotId,
    "The BLAKE3 identity of a canonical snapshot representation."
);
digest_id!(ObjectId, "The BLAKE3 identity of immutable object bytes.");

/// Failure to construct or parse a positive integer identifier.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IntegerIdParseError {
    /// The input is empty, contains non-digits, or has leading zeroes.
    #[error("integer IDs must use canonical unsigned decimal text")]
    NonCanonical,
    /// Zero is not a valid process or event sequence identifier.
    #[error("integer IDs must be greater than zero")]
    Zero,
    /// The decimal input exceeds the identifier's integer range.
    #[error("integer ID is outside the supported range")]
    OutOfRange,
}

fn validate_decimal(input: &str) -> Result<(), IntegerIdParseError> {
    if input.is_empty()
        || input.bytes().any(|byte| !byte.is_ascii_digit())
        || (input.len() > 1 && input.starts_with('0'))
    {
        return Err(IntegerIdParseError::NonCanonical);
    }
    Ok(())
}

/// A nonzero operating-system process identifier observed during capture.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessId(NonZeroU32);

impl ProcessId {
    /// Constructs a process identifier, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric process identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ProcessId {
    type Err = IntegerIdParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        validate_decimal(input)?;
        let value = input
            .parse::<u32>()
            .map_err(|_| IntegerIdParseError::OutOfRange)?;
        Self::new(value).ok_or(IntegerIdParseError::Zero)
    }
}

impl TryFrom<u32> for ProcessId {
    type Error = IntegerIdParseError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(IntegerIdParseError::Zero)
    }
}

impl From<ProcessId> for u32 {
    fn from(value: ProcessId) -> Self {
        value.get()
    }
}

/// A nonzero, monotonically increasing event position within one run.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSequence(NonZeroU64);

impl EventSequence {
    /// The first valid sequence number in a run.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Constructs a sequence, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric sequence value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next sequence, or `None` at `u64::MAX`.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::new(value),
            None => None,
        }
    }
}

impl fmt::Display for EventSequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for EventSequence {
    type Err = IntegerIdParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        validate_decimal(input)?;
        let value = input
            .parse::<u64>()
            .map_err(|_| IntegerIdParseError::OutOfRange)?;
        Self::new(value).ok_or(IntegerIdParseError::Zero)
    }
}

impl TryFrom<u64> for EventSequence {
    type Error = IntegerIdParseError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(IntegerIdParseError::Zero)
    }
}

impl From<EventSequence> for u64 {
    fn from(value: EventSequence) -> Self {
        value.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_record_ids_are_v7_and_ordered() {
        let mut previous = RunId::generate();
        for _ in 0..128 {
            let current = RunId::generate();
            assert_eq!(current.as_uuid().get_version(), Some(Version::SortRand));
            assert!(previous < current);
            previous = current;
        }
    }

    #[test]
    fn record_id_text_and_serde_are_strict() {
        let id = RunId::generate();
        let canonical = id.to_string();
        assert_eq!(canonical.parse::<RunId>().unwrap(), id);
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            format!("\"{canonical}\"")
        );
        assert_eq!(
            serde_json::from_str::<RunId>(&format!("\"{canonical}\"")).unwrap(),
            id
        );

        assert!(canonical.to_uppercase().parse::<RunId>().is_err());
        assert!(canonical.replace('-', "").parse::<RunId>().is_err());
        assert!(
            "550e8400-e29b-41d4-a716-446655440000"
                .parse::<RunId>()
                .is_err()
        );
    }

    #[test]
    fn digest_ids_are_exact_lowercase_hex() {
        for length in [0, 1, 31, 32, 63, 65, 128] {
            let bytes = vec![length as u8; length];
            let id = ObjectId::digest(&bytes);
            let encoded = id.to_string();
            assert_eq!(encoded.len(), 64);
            assert_eq!(encoded.parse::<ObjectId>().unwrap(), id);
            assert_eq!(
                serde_json::from_str::<ObjectId>(&format!("\"{encoded}\"")).unwrap(),
                id
            );
            assert!(encoded.to_uppercase().parse::<ObjectId>().is_err());
        }
        assert!("0".repeat(63).parse::<ObjectId>().is_err());
        assert!(format!("{}g", "0".repeat(63)).parse::<ObjectId>().is_err());
    }

    #[test]
    fn integer_identifiers_reject_noncanonical_and_zero_values() {
        for invalid in ["", "0", "00", "01", "+1", "-1", "1_0", " 1"] {
            assert!(
                invalid.parse::<ProcessId>().is_err(),
                "accepted {invalid:?}"
            );
            assert!(
                invalid.parse::<EventSequence>().is_err(),
                "accepted {invalid:?}"
            );
        }
        assert_eq!("42".parse::<ProcessId>().unwrap().get(), 42);
        assert_eq!(EventSequence::FIRST.checked_next().unwrap().get(), 2);
        assert_eq!(EventSequence::new(u64::MAX).unwrap().checked_next(), None);
        assert!(serde_json::from_str::<ProcessId>("0").is_err());
    }
}
