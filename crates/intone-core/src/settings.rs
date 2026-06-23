//! User settings and their secure on-disk storage.
//!
//! Settings live under the per-user XDG config directory. On Unix the file is written
//! `0600` and its directory `0700` so other users cannot read a user's configuration
//! (which may include exclusion patterns revealing app usage). No secrets are stored here;
//! networking is **off by default** (no telemetry / no cloud).

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::exclusions::ExclusionRule;

/// How much detail the screen reader announces by default.
///
/// Ordered `Low < Medium < High`, so announcement policy can compare levels
/// (`verbosity >= Verbosity::Medium`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
/// Speech output settings, applied to the speech engine by the platform back-end.
/// `rate`/`pitch`/`volume` are 0–100 (50 = normal; volume 100 = full).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Speech {
    /// Speaking rate, 0–100 (50 = normal).
    pub rate: u8,
    /// Voice pitch, 0–100 (50 = normal).
    pub pitch: u8,
    /// Volume, 0–100 (100 = full).
    pub volume: u8,
    /// Synthesis voice name (engine-specific); `None` = engine default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Language tag, e.g. `"en"` (BCP-47); `None` = engine default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// speech-dispatcher output module, e.g. `"espeak-ng"`; `None` = daemon default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_module: Option<String>,
    /// Voice names to cycle through with the in-app switch hotkey (Ctrl+Alt+V on Linux). Empty
    /// disables cycling. Each name is what `intone config voice <name>` would accept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rotation: Vec<String>,
    /// Map of language tag (e.g. `en`, `es`, `en-GB`) → voice name, used to auto-switch the voice
    /// to match the focused content's language. Empty disables auto-switching. Declared last so
    /// it serialises as the `[speech.by_language]` sub-table after the scalar fields.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_language: BTreeMap<String, String>,
    /// Map of context (`content` / `ui`) → voice name, to read application content and the
    /// reader's own meta-announcements (time, structure, navigation, hotkey feedback) in
    /// different voices. Empty disables context voices. Serialises as `[speech.by_context]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_context: BTreeMap<String, String>,
}

impl Default for Speech {
    fn default() -> Self {
        Self {
            rate: 50,
            pitch: 50,
            volume: 100,
            voice: None,
            language: None,
            output_module: None,
            rotation: Vec::new(),
            by_language: BTreeMap::new(),
            by_context: BTreeMap::new(),
        }
    }
}

/// The kind of thing being announced, used to choose a context-specific voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechContext {
    /// Application content being read: focus readout, caret/typed text, selections.
    Content,
    /// The reader's own meta-announcements: time, structure summary, navigation, hotkey feedback.
    Ui,
}

impl SpeechContext {
    /// The `by_context` map key for this context.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Ui => "ui",
        }
    }
}

/// Choose the configured voice for a content `locale`, if any.
///
/// `by_language` maps a language tag (`en`, `es`, `en-GB`, …) to a voice name; `locale` is the
/// focused content's language tag (lower/upper-case and `en-US`/`en_US` forms are both fine — the
/// caller normalises separators). Matching is case-insensitive and boundary-aware (so `en` matches
/// `en` and `en-GB` but not `eng`), preferring the **most specific** (longest) configured tag.
/// Returns `None` when nothing matches, so the caller keeps the current voice.
#[must_use]
pub fn voice_for_language<'a>(
    by_language: &'a BTreeMap<String, String>,
    locale: Option<&str>,
) -> Option<&'a str> {
    let locale = locale?.to_ascii_lowercase();
    by_language
        .iter()
        .filter(|(tag, _)| {
            let tag = tag.to_ascii_lowercase();
            locale == tag || locale.starts_with(&format!("{tag}-"))
        })
        .max_by_key(|(tag, _)| tag.len())
        .map(|(_, voice)| voice.as_str())
}

/// Resolve which voice an announcement should use, by precedence. For **content**: a
/// language-matched voice (most specific) wins, then the configured `content` context voice, then
/// the default voice. For **UI** meta-announcements: the `ui` context voice, then the default.
/// Returns `None` when nothing is configured, so the caller leaves the current voice unchanged.
#[must_use]
pub fn resolve_voice<'a>(
    speech: &'a Speech,
    context: SpeechContext,
    locale: Option<&str>,
) -> Option<&'a str> {
    let context_voice = || speech.by_context.get(context.key()).map(String::as_str);
    let default = || speech.voice.as_deref();
    match context {
        SpeechContext::Content => voice_for_language(&speech.by_language, locale)
            .or_else(context_voice)
            .or_else(default),
        SpeechContext::Ui => context_voice().or_else(default),
    }
}

/// Scalar fields are declared before the `[speech]` table and `exclusions` array so the value
/// serialises to valid TOML (values must precede tables/arrays-of-tables).
///
/// `Default` is derived: network off, [`Verbosity::Medium`], [`Speech`] defaults, no exclusions.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Default verbosity.
    pub verbosity: Verbosity,
    /// Whether any network feature is permitted. **Off by default** (no tracking).
    pub allow_network: bool,
    /// Whether braille output is enabled. **Off by default.**
    pub braille: bool,
    /// Speech output settings (rate, pitch, volume, voice, …).
    pub speech: Speech,
    /// User-defined exclusion rules.
    pub exclusions: Vec<ExclusionRule>,
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

/// The directory where intone stores per-user configuration.
///
/// # Errors
/// Returns [`Error::NoConfigDir`] if no config directory can be determined.
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "intone", "intone").ok_or(Error::NoConfigDir)?;
    Ok(dirs.config_dir().to_path_buf())
}

/// The path to intone's settings file.
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
        assert_eq!(back.speech.rate, s.speech.rate);
        assert_eq!(back.speech.volume, s.speech.volume);
        assert_eq!(back.allow_network, s.allow_network);
        assert_eq!(back.braille, s.braille);
        assert_eq!(back.verbosity, s.verbosity);
    }

    #[test]
    fn voice_for_language_prefers_most_specific_match() {
        use super::voice_for_language;
        use std::collections::BTreeMap;
        let map = BTreeMap::from([
            ("en".to_owned(), "Alan".to_owned()),
            ("en-GB".to_owned(), "Daniel".to_owned()),
            ("es".to_owned(), "Pedro".to_owned()),
        ]);
        // Most specific (longest) configured tag wins.
        assert_eq!(voice_for_language(&map, Some("en-GB")), Some("Daniel"));
        // Falls back to the broader tag when only it matches.
        assert_eq!(voice_for_language(&map, Some("en-US")), Some("Alan"));
        // Case-insensitive.
        assert_eq!(voice_for_language(&map, Some("ES-es")), Some("Pedro"));
        // No match / no locale / mere shared prefix without a boundary.
        assert_eq!(voice_for_language(&map, Some("de-DE")), None);
        assert_eq!(voice_for_language(&map, None), None);
        assert_eq!(voice_for_language(&map, Some("eng")), None);
    }

    #[test]
    fn resolve_voice_applies_precedence() {
        use super::{resolve_voice, Speech, SpeechContext};
        let mut speech = Speech {
            voice: Some("Default".to_owned()),
            ..Speech::default()
        };
        speech
            .by_context
            .insert("content".to_owned(), "ContentVoice".to_owned());
        speech
            .by_context
            .insert("ui".to_owned(), "UiVoice".to_owned());
        speech
            .by_language
            .insert("es".to_owned(), "Spanish".to_owned());

        // Content + matching language → language voice wins over the content context voice.
        assert_eq!(
            resolve_voice(&speech, SpeechContext::Content, Some("es-ES")),
            Some("Spanish")
        );
        // Content + no language match → content context voice.
        assert_eq!(
            resolve_voice(&speech, SpeechContext::Content, Some("en-US")),
            Some("ContentVoice")
        );
        // UI → ui context voice (language is irrelevant to chrome).
        assert_eq!(
            resolve_voice(&speech, SpeechContext::Ui, Some("es-ES")),
            Some("UiVoice")
        );

        // Fall back to the default voice when no context voice is set.
        speech.by_context.clear();
        assert_eq!(
            resolve_voice(&speech, SpeechContext::Ui, None),
            Some("Default")
        );
        assert_eq!(
            resolve_voice(&speech, SpeechContext::Content, Some("en")),
            Some("Default")
        );

        // Nothing configured at all → None (caller keeps the current voice).
        speech.voice = None;
        speech.by_language.clear();
        assert_eq!(
            resolve_voice(&speech, SpeechContext::Content, Some("en")),
            None
        );
        assert_eq!(resolve_voice(&speech, SpeechContext::Ui, None), None);
    }

    #[test]
    fn by_language_round_trips_as_a_subtable() {
        let mut s = Settings::default();
        s.speech.rotation = vec!["Alan".to_owned()];
        s.speech
            .by_language
            .insert("en".to_owned(), "Alan".to_owned());
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(
            back.speech.by_language.get("en").map(String::as_str),
            Some("Alan")
        );
    }
}
