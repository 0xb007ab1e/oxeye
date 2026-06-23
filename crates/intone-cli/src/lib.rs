//! `intone-cli` — the testable core of the `intone` configuration command.
//!
//! This library holds the **disk-free** rule-mutation and formatting logic so it can be unit
//! tested without touching the filesystem (the imperative shell — loading/saving settings and
//! printing — lives in `main.rs`). It depends only on [`intone_core`], keeping the core itself
//! free of any CLI dependency.

use anyhow::{ensure, Context, Result};
use intone_core::{Action, ExclusionEngine, ExclusionRule, Settings, Verbosity};

/// Add `rule` to `settings`, validating it first. Fails **closed**.
///
/// # Errors
/// Returns an error if the rule has no matchers (it would match every announcement) or if its
/// `name_regex` does not compile — in either case `settings` is left unchanged.
pub fn add_rule(settings: &mut Settings, rule: ExclusionRule) -> Result<()> {
    ensure!(
        rule.app.is_some() || rule.role.is_some() || rule.name_regex.is_some(),
        "refusing to add a rule with no matchers — it would match every announcement; \
         set at least one of --app / --role / --name-regex"
    );
    // Validate the regex by compiling the rule; refuse to persist a malformed rule.
    ExclusionEngine::compile(std::slice::from_ref(&rule)).context("invalid --name-regex")?;
    settings.exclusions.push(rule);
    Ok(())
}

/// Remove the rule numbered `position` (1-based, as printed by [`format_list`]).
///
/// # Errors
/// Returns an error if `position` is out of range; `settings` is left unchanged.
pub fn remove_rule(settings: &mut Settings, position: usize) -> Result<ExclusionRule> {
    let count = settings.exclusions.len();
    ensure!(
        position >= 1 && position <= count,
        "no rule #{position}; there are {count} rule(s) — see `intone exclusions list`"
    );
    Ok(settings.exclusions.remove(position - 1))
}

/// Render the configured rules as a numbered, human-readable list.
#[must_use]
pub fn format_list(settings: &Settings) -> String {
    if settings.exclusions.is_empty() {
        return "no exclusion rules configured".to_owned();
    }
    let mut lines = Vec::with_capacity(settings.exclusions.len());
    for (i, rule) in settings.exclusions.iter().enumerate() {
        let mut matchers = Vec::new();
        if let Some(app) = &rule.app {
            matchers.push(format!("app={app}"));
        }
        if let Some(role) = &rule.role {
            matchers.push(format!("role={role}"));
        }
        if let Some(re) = &rule.name_regex {
            matchers.push(format!("name~={re}"));
        }
        let matchers = if matchers.is_empty() {
            "(any)".to_owned()
        } else {
            matchers.join(" ")
        };
        lines.push(format!(
            "{}. [{}] {matchers}",
            i + 1,
            action_label(rule.action)
        ));
    }
    lines.join("\n")
}

/// Stable, lowercase label for an [`Action`] — used in listings and matching the `--action`
/// value names accepted on the command line.
#[must_use]
pub fn action_label(action: Action) -> &'static str {
    match action {
        Action::Suppress => "suppress",
        Action::Summarize => "summarize",
        Action::LowerPriority => "lower-priority",
    }
}

/// Stable, lowercase label for a [`Verbosity`] level (matches the CLI value names).
#[must_use]
pub fn verbosity_label(verbosity: Verbosity) -> &'static str {
    match verbosity {
        Verbosity::Low => "low",
        Verbosity::Medium => "medium",
        Verbosity::High => "high",
    }
}

/// Validate a 0–100 speech level (rate / pitch / volume). Fails **closed** above 100 rather
/// than silently clamping, so a typo is reported instead of quietly applied.
///
/// # Errors
/// Returns an error if `value` exceeds 100.
pub fn checked_level(value: u8) -> Result<u8> {
    ensure!(value <= 100, "level must be 0–100 (got {value})");
    Ok(value)
}

/// Interpret a CLI value for an optional speech setting (voice / language / output module):
/// the literal `default` clears it (revert to the engine default); anything else sets it.
#[must_use]
pub fn optional_setting(value: &str) -> Option<String> {
    match value {
        "default" => None,
        other => Some(other.to_owned()),
    }
}

/// A synthesis voice as listed for the user. Engine-agnostic — mirrors speech-dispatcher's voice
/// fields without depending on the SSIP types here, so the formatting stays unit-testable.
pub struct VoiceInfo {
    /// Voice name (what `intone config voice <name>` expects).
    pub name: String,
    /// Language tag, if the engine reports one.
    pub language: Option<String>,
    /// Dialect/variant, if any.
    pub dialect: Option<String>,
}

/// Cap on how many individual voices a filtered listing prints, so a language with hundreds of
/// speaker variants (e.g. espeak-ng) stays readable; the remainder is summarised as a count.
const VOICE_LIST_CAP: usize = 60;

/// The ` [lang-dialect]` suffix shown after a voice name (empty when no language is reported).
fn voice_tag(voice: &VoiceInfo) -> String {
    match (&voice.language, &voice.dialect) {
        (Some(lang), Some(dialect)) => format!(" [{lang}-{dialect}]"),
        (Some(lang), None) => format!(" [{lang}]"),
        _ => String::new(),
    }
}

/// Render output modules and the current module's voices for `intone voices list`.
///
/// Engines like espeak-ng expose tens of thousands of voices (every language × speaker variant),
/// so with no `language_filter` this prints a **per-language summary** (each language tag and its
/// voice count) and points the user at `--language`. With a filter, it lists the matching voices
/// by name (capped at [`VOICE_LIST_CAP`], with the remainder summarised). Voices are per the
/// active output module — switch it with `intone config module <name>` and re-run.
#[must_use]
pub fn format_voices(
    modules: &[String],
    voices: &[VoiceInfo],
    language_filter: Option<&str>,
) -> String {
    let module_list = if modules.is_empty() {
        "(none)".to_owned()
    } else {
        modules.join(", ")
    };
    let mut lines = vec![format!("output modules: {module_list}")];

    match language_filter {
        Some(filter) => {
            // Prefix match (case-insensitive): engines report full locales (e.g. `en-GB`,
            // `en-US`), so `en` should match them all, while `en-gb` narrows to one.
            let wanted = filter.to_ascii_lowercase();
            let matching: Vec<&VoiceInfo> = voices
                .iter()
                .filter(|v| {
                    v.language
                        .as_deref()
                        .is_some_and(|lang| lang.to_ascii_lowercase().starts_with(&wanted))
                })
                .collect();
            if matching.is_empty() {
                lines.push(format!(
                    "no voices for language '{filter}' in the current module"
                ));
            } else {
                lines.push(format!(
                    "voices for language '{filter}' ({}):",
                    matching.len()
                ));
                for voice in matching.iter().take(VOICE_LIST_CAP) {
                    lines.push(format!("  {}{}", voice.name, voice_tag(voice)));
                }
                if matching.len() > VOICE_LIST_CAP {
                    lines.push(format!("  … and {} more", matching.len() - VOICE_LIST_CAP));
                }
            }
        }
        None => {
            let mut by_language: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let mut untagged = 0usize;
            for voice in voices {
                match &voice.language {
                    Some(lang) => *by_language.entry(lang.clone()).or_default() += 1,
                    None => untagged += 1,
                }
            }
            lines.push(format!(
                "{} voices across {} languages — refine with `intone voices list --language <tag>`:",
                voices.len(),
                by_language.len()
            ));
            for (lang, count) in &by_language {
                lines.push(format!("  {lang} ({count})"));
            }
            if untagged > 0 {
                lines.push(format!("  (untagged) ({untagged})"));
            }
        }
    }
    lines.join("\n")
}

/// A short, human-readable summary of the current configuration.
#[must_use]
pub fn format_config(settings: &Settings) -> String {
    let network = if settings.allow_network {
        "allowed"
    } else {
        "off"
    };
    let braille = if settings.braille { "on" } else { "off" };
    let speech = &settings.speech;
    let or_default = |opt: &Option<String>| opt.clone().unwrap_or_else(|| "default".to_owned());
    let rotation = if speech.rotation.is_empty() {
        "(none)".to_owned()
    } else {
        speech.rotation.join(", ")
    };
    let by_language = if speech.by_language.is_empty() {
        "(none)".to_owned()
    } else {
        speech
            .by_language
            .iter()
            .map(|(tag, voice)| format!("{tag}→{voice}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "verbosity: {}\n\
         network: {network}\n\
         braille: {braille}\n\
         speech: rate {}, pitch {}, volume {}\n\
         voice: {}\n\
         language: {}\n\
         output module: {}\n\
         voice rotation: {rotation}\n\
         language voices: {by_language}\n\
         exclusion rules: {}",
        verbosity_label(settings.verbosity),
        speech.rate,
        speech.pitch,
        speech.volume,
        or_default(&speech.voice),
        or_default(&speech.language),
        or_default(&speech.output_module),
        settings.exclusions.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::{action_label, add_rule, format_list, remove_rule};
    use intone_core::{Action, ExclusionRule, Settings};

    fn rule(app: Option<&str>, name: Option<&str>, action: Action) -> ExclusionRule {
        ExclusionRule {
            app: app.map(str::to_owned),
            role: None,
            name_regex: name.map(str::to_owned),
            action,
        }
    }

    #[test]
    fn add_then_remove_roundtrips() {
        let mut s = Settings::default();
        add_rule(&mut s, rule(Some("noisyapp"), None, Action::Suppress)).unwrap();
        assert_eq!(s.exclusions.len(), 1);
        let removed = remove_rule(&mut s, 1).unwrap();
        assert_eq!(removed.action, Action::Suppress);
        assert!(s.exclusions.is_empty());
    }

    #[test]
    fn add_rejects_empty_matcher() {
        let mut s = Settings::default();
        assert!(add_rule(&mut s, rule(None, None, Action::Suppress)).is_err());
        assert!(
            s.exclusions.is_empty(),
            "a rejected rule must not be stored"
        );
    }

    #[test]
    fn add_rejects_invalid_regex() {
        let mut s = Settings::default();
        assert!(add_rule(&mut s, rule(None, Some("(unclosed"), Action::Summarize)).is_err());
        assert!(s.exclusions.is_empty());
    }

    #[test]
    fn remove_out_of_range_is_error() {
        let mut s = Settings::default();
        add_rule(&mut s, rule(Some("a"), None, Action::Suppress)).unwrap();
        assert!(
            remove_rule(&mut s, 0).is_err(),
            "1-based: index 0 is invalid"
        );
        assert!(remove_rule(&mut s, 2).is_err(), "out of range");
        assert_eq!(s.exclusions.len(), 1, "failed removals leave rules intact");
    }

    #[test]
    fn format_list_numbers_rules_and_handles_empty() {
        let mut s = Settings::default();
        assert!(format_list(&s).contains("no exclusion rules"));
        add_rule(
            &mut s,
            rule(Some("web"), Some("(?i)cookie"), Action::Summarize),
        )
        .unwrap();
        let listed = format_list(&s);
        assert!(listed.contains("1."), "rules are numbered");
        assert!(listed.contains("summarize"));
        assert!(listed.contains("app=web"));
        assert!(listed.contains("name~=(?i)cookie"));
    }

    #[test]
    fn action_labels_are_stable() {
        assert_eq!(action_label(Action::Suppress), "suppress");
        assert_eq!(action_label(Action::Summarize), "summarize");
        assert_eq!(action_label(Action::LowerPriority), "lower-priority");
    }

    #[test]
    fn verbosity_labels_are_stable() {
        use intone_core::Verbosity;
        assert_eq!(super::verbosity_label(Verbosity::Low), "low");
        assert_eq!(super::verbosity_label(Verbosity::Medium), "medium");
        assert_eq!(super::verbosity_label(Verbosity::High), "high");
    }

    #[test]
    fn config_summary_reports_verbosity_and_counts() {
        let mut s = Settings::default();
        add_rule(&mut s, rule(Some("a"), None, Action::Suppress)).unwrap();
        let out = super::format_config(&s);
        assert!(out.contains("verbosity: medium"), "default verbosity");
        assert!(out.contains("network: off"), "network off by default");
        assert!(out.contains("braille: off"), "braille off by default");
        assert!(out.contains("exclusion rules: 1"));
    }

    #[test]
    fn config_summary_reports_speech_defaults() {
        let out = super::format_config(&Settings::default());
        assert!(
            out.contains("speech: rate 50, pitch 50, volume 100"),
            "speech defaults shown"
        );
        assert!(out.contains("voice: default"), "voice unset shows default");
        assert!(out.contains("output module: default"));
    }

    #[test]
    fn config_summary_reports_a_set_voice() {
        let mut s = Settings::default();
        s.speech.voice = Some("Alan".to_owned());
        s.speech.rate = 70;
        let out = super::format_config(&s);
        assert!(out.contains("voice: Alan"));
        assert!(out.contains("rate 70"));
    }

    #[test]
    fn config_summary_reports_voice_rotation() {
        let mut s = Settings::default();
        assert!(
            super::format_config(&s).contains("voice rotation: (none)"),
            "empty rotation shows (none)"
        );
        s.speech.rotation = vec!["Alan".to_owned(), "Klaus".to_owned()];
        assert!(super::format_config(&s).contains("voice rotation: Alan, Klaus"));
    }

    #[test]
    fn config_summary_reports_language_voices() {
        let mut s = Settings::default();
        assert!(super::format_config(&s).contains("language voices: (none)"));
        s.speech
            .by_language
            .insert("en".to_owned(), "Alan".to_owned());
        s.speech
            .by_language
            .insert("es".to_owned(), "Pedro".to_owned());
        // BTreeMap keeps tags sorted, so the display order is stable.
        assert!(super::format_config(&s).contains("language voices: en→Alan, es→Pedro"));
    }

    #[test]
    fn checked_level_accepts_0_to_100_and_rejects_above() {
        assert_eq!(super::checked_level(0).unwrap(), 0);
        assert_eq!(super::checked_level(100).unwrap(), 100);
        assert!(super::checked_level(101).is_err(), "fails closed above 100");
    }

    #[test]
    fn optional_setting_treats_default_as_clear() {
        assert_eq!(super::optional_setting("default"), None);
        assert_eq!(super::optional_setting("Alan"), Some("Alan".to_owned()));
    }

    fn voice(name: &str, language: Option<&str>, dialect: Option<&str>) -> super::VoiceInfo {
        super::VoiceInfo {
            name: name.to_owned(),
            language: language.map(str::to_owned),
            dialect: dialect.map(str::to_owned),
        }
    }

    #[test]
    fn format_voices_summarizes_by_language_without_filter() {
        use super::format_voices;
        let modules = vec!["espeak-ng".to_owned(), "piper".to_owned()];
        let voices = vec![
            voice("Alan", Some("en"), Some("us")),
            voice("Daniel", Some("en"), Some("gb")),
            voice("Klaus", Some("de"), None),
        ];
        let out = format_voices(&modules, &voices, None);
        assert!(out.contains("output modules: espeak-ng, piper"));
        assert!(
            out.contains("3 voices across 2 languages"),
            "summary header"
        );
        assert!(out.contains("en (2)"), "english count");
        assert!(out.contains("de (1)"), "german count");
    }

    #[test]
    fn format_voices_filters_by_language_prefix_case_insensitively() {
        use super::format_voices;
        // Full locales (en-GB / en-US) must match the bare prefix `EN`, German must not.
        let voices = vec![
            voice("Daniel", Some("en-GB"), None),
            voice("Alan", Some("en-US"), None),
            voice("Klaus", Some("de-DE"), None),
        ];
        let out = format_voices(&[], &voices, Some("EN"));
        assert!(
            out.contains("voices for language 'EN' (2):"),
            "both English locales"
        );
        assert!(out.contains("Daniel [en-GB]"));
        assert!(out.contains("Alan [en-US]"));
        assert!(!out.contains("Klaus"), "non-matching language excluded");
    }

    #[test]
    fn format_voices_reports_no_match_for_unknown_language() {
        use super::format_voices;
        let voices = vec![voice("Alan", Some("en"), None)];
        let out = format_voices(&[], &voices, Some("zz"));
        assert!(out.contains("no voices for language 'zz'"));
    }

    #[test]
    fn format_voices_handles_empty() {
        use super::format_voices;
        let out = format_voices(&[], &[], None);
        assert!(out.contains("output modules: (none)"));
        assert!(out.contains("0 voices across 0 languages"));
    }
}
