use rewind_domain::InputRecordingPolicy;
use rewind_platform::{DirectoryRoot, read_regular_file_beneath};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str;
use std::time::Duration;
use thiserror::Error;

#[cfg(test)]
use std::fs;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Config {
    pub(crate) workspace: WorkspaceConfig,
    pub(crate) capture: CaptureConfig,
    pub(crate) storage: StorageConfig,
    pub(crate) privacy: PrivacyConfig,
    pub(crate) replay: ReplayConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct WorkspaceConfig {
    pub(crate) ignore: Vec<String>,
    pub(crate) max_file_size: ByteSize,
    pub(crate) follow_symlinks: bool,
    pub(crate) binary_files: BinaryFileBehavior,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CaptureConfig {
    pub(crate) record_input: InputRecordingPolicy,
    pub(crate) checkpoint_debounce: DurationValue,
    pub(crate) checkpoint_min_interval: DurationValue,
    pub(crate) checkpoint_max_interval: DurationValue,
    pub(crate) maximum_pending_dirty_paths: usize,
    pub(crate) process_poll_interval: DurationValue,
    pub(crate) terminal_chunk_size: ByteSize,
    pub(crate) terminal_max_bytes: ByteSize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StorageConfig {
    pub(crate) max_run_size: ByteSize,
    pub(crate) compression: CompressionPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PrivacyConfig {
    pub(crate) capture_environment: bool,
    pub(crate) environment_allowlist: Vec<String>,
    pub(crate) environment_denylist: Vec<String>,
    pub(crate) excluded_paths: Vec<String>,
    pub(crate) redact_exports: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ReplayConfig {
    pub(crate) terminal_cache: ByteSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BinaryFileBehavior {
    Record,
    Exclude,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompressionPolicy {
    Never,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ByteSize(u64);

impl ByteSize {
    pub(crate) const fn bytes(self) -> u64 {
        self.0
    }
}

impl Serialize for ByteSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SizeVisitor;
        impl Visitor<'_> for SizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a size such as '64 MiB' using B, KiB, MiB, or GiB")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                parse_size(value).map(ByteSize).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(SizeVisitor)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct DurationValue(Duration);

impl DurationValue {
    pub(crate) const fn duration(self) -> Duration {
        self.0
    }
}

impl Serialize for DurationValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let milliseconds = u64::try_from(self.0.as_millis()).map_err(serde::ser::Error::custom)?;
        serializer.serialize_u64(milliseconds)
    }
}

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DurationVisitor;
        impl Visitor<'_> for DurationVisitor {
            type Value = DurationValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a duration such as '750 ms', '2 s', or '1 min'")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                parse_duration(value).map(DurationValue).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(DurationVisitor)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("cannot inspect configuration {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: rewind_platform::FileSystemError,
    },
    #[error("configuration {path} is not valid UTF-8: {source}")]
    Encoding {
        path: PathBuf,
        #[source]
        source: str::Utf8Error,
    },
    #[error("configuration path has no UTF-8 file name: {0}")]
    InvalidPath(PathBuf),
    #[error("configuration {path} is {actual} bytes; maximum is {maximum}")]
    TooLarge {
        path: PathBuf,
        actual: u64,
        maximum: u64,
    },
    #[error("configuration {path} is invalid: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("checkpoint_min_interval must not exceed checkpoint_max_interval")]
    CheckpointIntervals,
    #[error("checkpoint_debounce must not exceed checkpoint_max_interval")]
    CheckpointDebounce,
    #[error("capture.maximum_pending_dirty_paths must be greater than zero")]
    PendingDirtyPaths,
    #[error("terminal_chunk_size must not exceed terminal_max_bytes")]
    TerminalLimits,
    #[error(
        "follow_symlinks=true is unsupported because snapshot scans must not escape the workspace"
    )]
    FollowSymlinks,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialConfig {
    workspace: PartialWorkspace,
    capture: PartialCapture,
    storage: PartialStorage,
    privacy: PartialPrivacy,
    replay: PartialReplay,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialWorkspace {
    ignore: Option<Vec<String>>,
    max_file_size: Option<ByteSize>,
    follow_symlinks: Option<bool>,
    binary_files: Option<BinaryFileBehavior>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialCapture {
    record_input: Option<InputRecordingPolicy>,
    checkpoint_debounce: Option<DurationValue>,
    checkpoint_min_interval: Option<DurationValue>,
    checkpoint_max_interval: Option<DurationValue>,
    maximum_pending_dirty_paths: Option<usize>,
    process_poll_interval: Option<DurationValue>,
    terminal_chunk_size: Option<ByteSize>,
    terminal_max_bytes: Option<ByteSize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialStorage {
    max_run_size: Option<ByteSize>,
    compression: Option<CompressionPolicy>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialPrivacy {
    capture_environment: Option<bool>,
    environment_allowlist: Option<Vec<String>>,
    environment_denylist: Option<Vec<String>>,
    excluded_paths: Option<Vec<String>>,
    redact_exports: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialReplay {
    terminal_cache: Option<ByteSize>,
}

impl<'de> Deserialize<'de> for BinaryFileBehavior {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_enum(deserializer, "record or exclude", |value| match value {
            "record" => Some(Self::Record),
            "exclude" => Some(Self::Exclude),
            _ => None,
        })
    }
}

impl<'de> Deserialize<'de> for CompressionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_enum(deserializer, "never", |value| match value {
            "never" => Some(Self::Never),
            _ => None,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: WorkspaceConfig {
                ignore: Vec::new(),
                max_file_size: ByteSize(64 * 1024 * 1024),
                follow_symlinks: false,
                binary_files: BinaryFileBehavior::Record,
            },
            capture: CaptureConfig {
                record_input: InputRecordingPolicy::Auto,
                checkpoint_debounce: DurationValue(Duration::from_millis(750)),
                checkpoint_min_interval: DurationValue(Duration::from_secs(2)),
                checkpoint_max_interval: DurationValue(Duration::from_secs(60)),
                maximum_pending_dirty_paths: 10_000,
                process_poll_interval: DurationValue(Duration::from_millis(250)),
                terminal_chunk_size: ByteSize(64 * 1024),
                terminal_max_bytes: ByteSize(2 * 1024 * 1024 * 1024),
            },
            storage: StorageConfig {
                max_run_size: ByteSize(10 * 1024 * 1024 * 1024),
                // ponytail: RWOB v1 stores raw bytes; add compression only
                // when measured workloads justify its dependency and policy.
                compression: CompressionPolicy::Never,
            },
            privacy: PrivacyConfig {
                capture_environment: false,
                environment_allowlist: Vec::new(),
                environment_denylist: vec![
                    "*TOKEN*".to_owned(),
                    "*SECRET*".to_owned(),
                    "*PASSWORD*".to_owned(),
                ],
                excluded_paths: vec![
                    ".env".to_owned(),
                    "**/.env".to_owned(),
                    ".env.*".to_owned(),
                    "**/.env.*".to_owned(),
                    "*.pem".to_owned(),
                    "**/*.pem".to_owned(),
                    "*.key".to_owned(),
                    "**/*.key".to_owned(),
                    "id_rsa".to_owned(),
                    "**/id_rsa".to_owned(),
                    "id_ed25519".to_owned(),
                    "**/id_ed25519".to_owned(),
                ],
                redact_exports: true,
            },
            replay: ReplayConfig {
                terminal_cache: ByteSize(64 * 1024 * 1024),
            },
        }
    }
}

impl Config {
    pub(crate) fn load(workspace: &Path, user_config: &Path) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        config.apply(read_partial(user_config)?);
        config.apply(read_partial(&workspace.join(".rewind.toml"))?);
        config.validate()?;
        Ok(config)
    }

    fn apply(&mut self, partial: PartialConfig) {
        apply(&mut self.workspace.ignore, partial.workspace.ignore);
        apply(
            &mut self.workspace.max_file_size,
            partial.workspace.max_file_size,
        );
        apply(
            &mut self.workspace.follow_symlinks,
            partial.workspace.follow_symlinks,
        );
        apply(
            &mut self.workspace.binary_files,
            partial.workspace.binary_files,
        );
        apply(&mut self.capture.record_input, partial.capture.record_input);
        apply(
            &mut self.capture.checkpoint_debounce,
            partial.capture.checkpoint_debounce,
        );
        apply(
            &mut self.capture.checkpoint_min_interval,
            partial.capture.checkpoint_min_interval,
        );
        apply(
            &mut self.capture.checkpoint_max_interval,
            partial.capture.checkpoint_max_interval,
        );
        apply(
            &mut self.capture.maximum_pending_dirty_paths,
            partial.capture.maximum_pending_dirty_paths,
        );
        apply(
            &mut self.capture.process_poll_interval,
            partial.capture.process_poll_interval,
        );
        apply(
            &mut self.capture.terminal_chunk_size,
            partial.capture.terminal_chunk_size,
        );
        apply(
            &mut self.capture.terminal_max_bytes,
            partial.capture.terminal_max_bytes,
        );
        apply(&mut self.storage.max_run_size, partial.storage.max_run_size);
        apply(&mut self.storage.compression, partial.storage.compression);
        apply(
            &mut self.privacy.capture_environment,
            partial.privacy.capture_environment,
        );
        apply(
            &mut self.privacy.environment_allowlist,
            partial.privacy.environment_allowlist,
        );
        apply(
            &mut self.privacy.environment_denylist,
            partial.privacy.environment_denylist,
        );
        apply(
            &mut self.privacy.excluded_paths,
            partial.privacy.excluded_paths,
        );
        apply(
            &mut self.privacy.redact_exports,
            partial.privacy.redact_exports,
        );
        apply(
            &mut self.replay.terminal_cache,
            partial.replay.terminal_cache,
        );
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.workspace.follow_symlinks {
            return Err(ConfigError::FollowSymlinks);
        }
        if self.capture.checkpoint_min_interval > self.capture.checkpoint_max_interval {
            return Err(ConfigError::CheckpointIntervals);
        }
        if self.capture.checkpoint_debounce > self.capture.checkpoint_max_interval {
            return Err(ConfigError::CheckpointDebounce);
        }
        if self.capture.maximum_pending_dirty_paths == 0 {
            return Err(ConfigError::PendingDirtyPaths);
        }
        if self.capture.terminal_chunk_size > self.capture.terminal_max_bytes {
            return Err(ConfigError::TerminalLimits);
        }
        Ok(())
    }
}

fn apply<T>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn read_partial(path: &Path) -> Result<PartialConfig, ConfigError> {
    if !path.try_exists().map_err(|source| ConfigError::Inspect {
        path: path.to_path_buf(),
        source,
    })? {
        return Ok(PartialConfig::default());
    }
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::InvalidPath(path.to_path_buf()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ConfigError::InvalidPath(path.to_path_buf()))?;
    let root = DirectoryRoot::open(parent).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let read =
        read_regular_file_beneath(&root, name, MAX_CONFIG_BYTES).map_err(
            |source| match source {
                rewind_platform::FileSystemError::FileTooLarge {
                    actual, maximum, ..
                } => ConfigError::TooLarge {
                    path: path.to_path_buf(),
                    actual,
                    maximum,
                },
                source => ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                },
            },
        )?;
    let text = str::from_utf8(&read.bytes).map_err(|source| ConfigError::Encoding {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn parse_size(value: &str) -> Result<u64, String> {
    let (number, unit) = split_quantity(value)?;
    let multiplier = match unit {
        "B" => 1,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        _ => return Err("size unit must be B, KiB, MiB, or GiB".to_owned()),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "size is too large".to_owned())
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let (number, unit) = split_quantity(value)?;
    let milliseconds = match unit {
        "ms" => number,
        "s" => number
            .checked_mul(1_000)
            .ok_or_else(|| "duration is too large".to_owned())?,
        "min" => number
            .checked_mul(60_000)
            .ok_or_else(|| "duration is too large".to_owned())?,
        _ => return Err("duration unit must be ms, s, or min".to_owned()),
    };
    Ok(Duration::from_millis(milliseconds))
}

fn split_quantity(value: &str) -> Result<(u64, &str), String> {
    let Some((number, unit)) = value.split_once(' ') else {
        return Err("value must contain an integer, one space, and a unit".to_owned());
    };
    if number.is_empty()
        || unit.is_empty()
        || unit.contains(' ')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("value must contain an integer, one space, and a unit".to_owned());
    }
    let number = number
        .parse::<u64>()
        .map_err(|_| "quantity is outside the supported range".to_owned())?;
    if number == 0 {
        return Err("quantity must be greater than zero".to_owned());
    }
    Ok((number, unit))
}

fn deserialize_enum<'de, D, T>(
    deserializer: D,
    expected: &'static str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse(&value).ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(&value), &expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_units_and_unknown_fields_fail() {
        for invalid in ["64MB", "64  MiB", "64 MB", "0 MiB", "-1 MiB", "1.5 MiB"] {
            assert!(parse_size(invalid).is_err(), "accepted {invalid}");
        }
        assert_eq!(parse_size("64 MiB").unwrap(), 64 * 1024 * 1024);
        assert_eq!(
            parse_duration("750 ms").unwrap(),
            Duration::from_millis(750)
        );
        assert!(toml::from_str::<PartialConfig>("[capture]\ncheckpont_debounce = '1 s'").is_err());
        assert!(toml::from_str::<PartialConfig>("[storage]\ncompression = 'auto'").is_err());
    }

    #[test]
    fn project_configuration_overrides_user_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let user = temp.path().join("user.toml");
        fs::write(
            &user,
            "[capture]\nrecord_input = 'never'\ncheckpoint_max_interval = '30 s'\n",
        )
        .unwrap();
        fs::write(
            workspace.join(".rewind.toml"),
            "[capture]\nrecord_input = 'always'\n[workspace]\nmax_file_size = '1 MiB'\n",
        )
        .unwrap();
        let config = Config::load(&workspace, &user).unwrap();
        assert_eq!(config.capture.record_input, InputRecordingPolicy::Always);
        assert_eq!(
            config.capture.checkpoint_max_interval.duration(),
            Duration::from_secs(30)
        );
        assert_eq!(config.workspace.max_file_size.bytes(), 1024 * 1024);
    }

    #[test]
    fn unsafe_symlink_following_is_rejected() {
        let partial: PartialConfig =
            toml::from_str("[workspace]\nfollow_symlinks = true\n").unwrap();
        let mut config = Config::default();
        config.apply(partial);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::FollowSymlinks)
        ));
    }

    #[test]
    fn configuration_reads_are_bounded_and_do_not_follow_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let oversized = temp.path().join("oversized.toml");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        assert!(matches!(
            Config::load(&workspace, &oversized),
            Err(ConfigError::TooLarge { .. })
        ));

        let outside = temp.path().join("outside.toml");
        fs::write(&outside, "[capture]\nrecord_input = 'never'\n").unwrap();
        let linked = temp.path().join("linked.toml");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        assert!(matches!(
            Config::load(&workspace, &linked),
            Err(ConfigError::Read { .. })
        ));
    }
}
