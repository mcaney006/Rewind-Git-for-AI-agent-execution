use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Failure to parse a durable numeric time value.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TimeParseError {
    /// The input was not the canonical decimal representation of the value.
    #[error("time values must use canonical decimal text")]
    NonCanonical,
    /// The input was outside the represented integer range.
    #[error("time value is outside the supported range")]
    OutOfRange,
}

/// A wall-clock instant represented as milliseconds from the Unix epoch.
///
/// Wall-clock values are presentation metadata and never define event order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Constructs a timestamp from signed Unix epoch milliseconds.
    #[must_use]
    pub const fn from_unix_milliseconds(value: i64) -> Self {
        Self(value)
    }

    /// Returns signed Unix epoch milliseconds.
    #[must_use]
    pub const fn as_unix_milliseconds(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Timestamp {
    type Err = TimeParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let value = input
            .parse::<i64>()
            .map_err(|_| TimeParseError::OutOfRange)?;
        if value.to_string() != input {
            return Err(TimeParseError::NonCanonical);
        }
        Ok(Self(value))
    }
}

/// Elapsed monotonic time represented as nanoseconds from run start.
///
/// The same type is used for event offsets and completed run durations.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MonotonicDuration(u64);

impl MonotonicDuration {
    /// A zero offset or elapsed duration.
    pub const ZERO: Self = Self(0);

    /// Constructs a duration from nanoseconds.
    #[must_use]
    pub const fn from_nanoseconds(value: u64) -> Self {
        Self(value)
    }

    /// Returns the represented nanoseconds.
    #[must_use]
    pub const fn as_nanoseconds(self) -> u64 {
        self.0
    }

    /// Returns the standard-library duration.
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_nanos(self.0)
    }
}

impl fmt::Display for MonotonicDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for MonotonicDuration {
    type Err = TimeParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let value = input
            .parse::<u64>()
            .map_err(|_| TimeParseError::OutOfRange)?;
        if value.to_string() != input {
            return Err(TimeParseError::NonCanonical);
        }
        Ok(Self(value))
    }
}

impl TryFrom<Duration> for MonotonicDuration {
    type Error = TimeParseError;

    fn try_from(value: Duration) -> Result<Self, Self::Error> {
        let nanoseconds =
            u64::try_from(value.as_nanos()).map_err(|_| TimeParseError::OutOfRange)?;
        Ok(Self(nanoseconds))
    }
}

impl From<MonotonicDuration> for Duration {
    fn from(value: MonotonicDuration) -> Self {
        value.as_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_text_is_canonical_and_round_trips() {
        for milliseconds in [i64::MIN, -1, 0, 1, i64::MAX] {
            let timestamp = Timestamp::from_unix_milliseconds(milliseconds);
            assert_eq!(
                timestamp.to_string().parse::<Timestamp>().unwrap(),
                timestamp
            );
            assert_eq!(
                serde_json::from_str::<Timestamp>(&serde_json::to_string(&timestamp).unwrap())
                    .unwrap(),
                timestamp
            );
        }
        for invalid in ["", "+1", "00", "01", "-0", " 1"] {
            assert!(invalid.parse::<Timestamp>().is_err());
            assert!(invalid.parse::<MonotonicDuration>().is_err());
        }
    }

    #[test]
    fn duration_conversion_rejects_values_over_u64_nanoseconds() {
        assert_eq!(
            MonotonicDuration::try_from(Duration::from_nanos(42))
                .unwrap()
                .as_nanoseconds(),
            42
        );
        assert!(MonotonicDuration::try_from(Duration::from_secs(u64::MAX)).is_err());
    }
}
