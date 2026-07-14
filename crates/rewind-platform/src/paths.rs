use std::env;
use std::path::PathBuf;
use thiserror::Error;

/// Platform-conventional Rewind data and configuration locations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationPaths {
    /// Potentially large database, object, run, and workspace storage.
    pub data_home: PathBuf,
    /// User-level TOML configuration file.
    pub user_config: PathBuf,
}

/// Failure to resolve a platform-conventional application path.
#[derive(Debug, Error)]
pub enum PathConventionError {
    /// Neither an override nor a usable home-directory environment variable exists.
    #[error("cannot resolve Rewind storage: set REWIND_HOME or HOME")]
    MissingHome,
}

/// Resolves Rewind paths once from explicit overrides and platform conventions.
pub fn application_paths() -> Result<ApplicationPaths, PathConventionError> {
    if let Some(data_home) = env::var_os("REWIND_HOME") {
        let data_home = PathBuf::from(data_home);
        return Ok(ApplicationPaths {
            user_config: data_home.join("config.toml"),
            data_home,
        });
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(PathConventionError::MissingHome)?;

    #[cfg(target_os = "macos")]
    {
        let data_home = home.join("Library/Application Support/Rewind");
        Ok(ApplicationPaths {
            user_config: data_home.join("config.toml"),
            data_home,
        })
    }

    #[cfg(target_os = "linux")]
    {
        let data_home = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("rewind");
        let user_config = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("rewind/config.toml");
        Ok(ApplicationPaths {
            data_home,
            user_config,
        })
    }
}
