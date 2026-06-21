//! User settings and their secure on-disk storage.
//!
//! Settings live under the per-user XDG config directory. On Unix the file is written
//! `0600` and its directory `0700` so other users cannot read a user's configuration
//! (which may include exclusion patterns revealing app usage). No secrets are stored here;
//! networking is **off by default** (no telemetry / no cloud).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::exclusions::ExclusionRule;

/// How much detail the screen reader announces by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verbosity {
    /// Minimal announcements.
    Low,
    /// Balanced default.
    Medium,
    /// Maximal detail.
    High,
}

impl Default for Verbosity {
    fn default() -> Self {
        Self::Medium
    }
}

/// User-configurable settings.
///
/// Scalar fields are declared before the `exclusions` array so the value serialises to
/// valid TOML (values must precede tables/arrays-of-tables).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Speech rate, 0–100 (engine-relative).
    pub speech_rate: u8,
    /// Default verbosity.
    pub verbosity: Verbosity,
    /// Whether any network feature is permitted. **Off by default** (no tracking).
    pub allow_network: bool,
    /// User-defined exclusion rules.
    pub exclusions: Vec<ExclusionRule>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            speech_rate: 50,
            verbosity: Verbosity::default(),
            allow_network: false,
            exclusions: Vec::new(),
        }
    }
}

impl Settings {
    /// Load settings from the user's config file, returning defaults if it does not exist.
    ///
    /// # Errors
    /// Returns an error if the config directory cannot be determined, the file cannot be
    /// read (other than "not found"), or its contents cannot be parsed.
    pub fn load() -> Result<Self> {
        let path = config_file()?;
        match std::fs::read_to_string(&path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Persist settings to the user's config file with hardened permissions.
    ///
    /// # Errors
    /// Returns an error if the config directory cannot be determined or created, the file
    /// cannot be written, or its permissions cannot be hardened.
    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        harden_dir(&dir);

        let path = config_file()?;
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&path, contents).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        harden_file(&path)
    }
}

/// The directory where oxeye stores per-user configuration.
///
/// # Errors
/// Returns [`Error::NoConfigDir`] if no config directory can be determined.
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "oxeye", "oxeye").ok_or(Error::NoConfigDir)?;
    Ok(dirs.config_dir().to_path_buf())
}

/// The path to oxeye's settings file.
///
/// # Errors
/// Returns [`Error::NoConfigDir`] if no config directory can be determined.
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("settings.toml"))
}

#[cfg(unix)]
fn harden_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // Best-effort: the settings file itself is hardened strictly via `harden_file`.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(unix)]
fn harden_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        Error::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn harden_dir(_path: &Path) {}

#[cfg(not(unix))]
fn harden_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Settings, Verbosity};

    #[test]
    fn defaults_are_private_and_offline() {
        let s = Settings::default();
        assert!(
            !s.allow_network,
            "network must be off by default (no tracking)"
        );
        assert_eq!(s.verbosity, Verbosity::Medium);
    }

    #[test]
    fn round_trips_through_toml() {
        let s = Settings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.speech_rate, s.speech_rate);
        assert_eq!(back.allow_network, s.allow_network);
        assert_eq!(back.verbosity, s.verbosity);
    }
}
