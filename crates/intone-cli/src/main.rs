//! `intone` — command-line configuration manager for the intone screen reader.
//!
//! Currently manages user-defined **exclusion rules** (the rules that tell the reader to
//! suppress, summarise, or de-prioritise announcements). The disk-free logic lives in the
//! `intone_cli` library; this binary is the imperative shell: parse args, load/save settings,
//! print results.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use intone_core::{Action, ExclusionRule, Settings, Verbosity};
use ssip_client_async::fifo::synchronous::Builder as SsipBuilder;
use ssip_client_async::{ClientName, Response};

/// Configure the intone screen reader.
#[derive(Parser)]
#[command(name = "intone", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage user-defined exclusion rules.
    Exclusions {
        #[command(subcommand)]
        command: ExclusionsCommand,
    },
    /// View or change general configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Discover speech voices and output modules from speech-dispatcher.
    Voices {
        #[command(subcommand)]
        command: VoicesCommand,
    },
}

#[derive(Subcommand)]
enum VoicesCommand {
    /// List installed output modules and the current module's synthesis voices.
    List {
        /// Show only voices for this language tag (e.g. `en`); omit for a per-language summary.
        #[arg(long)]
        language: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Show the current configuration.
    Show,
    /// Set the default announcement verbosity.
    Verbosity {
        /// How much detail to announce.
        level: VerbosityArg,
    },
    /// Turn braille output on or off.
    Braille {
        /// Whether braille output is enabled.
        state: Toggle,
    },
    /// Set the synthesis voice (engine-specific name; `default` reverts to the engine default).
    Voice {
        /// Voice name, or `default` to clear (list installed voices with `spd-say -L`).
        name: String,
    },
    /// Set the speech-dispatcher output module (e.g. `espeak-ng`, `piper`; `default` to clear).
    Module {
        /// Output-module name, or `default` to clear.
        name: String,
    },
    /// Set the speech language as a BCP-47 tag (e.g. `en`, `es`; `default` to clear).
    Language {
        /// Language tag, or `default` to clear.
        tag: String,
    },
    /// Set the speaking rate (0–100; 50 = normal).
    Rate {
        /// Rate, 0–100.
        value: u8,
    },
    /// Set the voice pitch (0–100; 50 = normal).
    Pitch {
        /// Pitch, 0–100.
        value: u8,
    },
    /// Set the volume (0–100; 100 = full).
    Volume {
        /// Volume, 0–100.
        value: u8,
    },
    /// Set the voices the in-app switch hotkey (Ctrl+Alt+V) cycles through; omit all to clear.
    Rotation {
        /// Voice names in cycle order (space-separated); none clears the rotation.
        names: Vec<String>,
    },
    /// Map a language to a voice, for automatic switching by content language (Linux).
    VoiceLang {
        /// Language tag (e.g. `en`, `es`, `en-GB`).
        tag: String,
        /// Voice name for that language, or `default` to remove the mapping.
        voice: String,
    },
    /// Set a voice for content vs the reader's own UI/meta announcements.
    VoiceContext {
        /// Which announcements: `content` (what's read) or `ui` (time/structure/navigation/…).
        context: ContextArg,
        /// Voice name for that context, or `default` to remove the mapping.
        voice: String,
    },
}

/// CLI mirror of the `content` / `ui` speech contexts (keeps clap out of `intone-core`).
#[derive(Clone, Copy, ValueEnum)]
enum ContextArg {
    /// Application content being read.
    Content,
    /// The reader's own meta-announcements.
    Ui,
}

impl ContextArg {
    /// The `by_context` map key.
    fn key(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Ui => "ui",
        }
    }
}

/// An on/off switch for a boolean setting.
#[derive(Clone, Copy, ValueEnum)]
enum Toggle {
    /// Enable the setting.
    On,
    /// Disable the setting.
    Off,
}

impl From<Toggle> for bool {
    fn from(toggle: Toggle) -> Self {
        matches!(toggle, Toggle::On)
    }
}

/// CLI mirror of [`intone_core::Verbosity`].
#[derive(Clone, Copy, ValueEnum)]
enum VerbosityArg {
    /// Just the essential label.
    Low,
    /// Label and role (the default).
    Medium,
    /// Label, role, and owning application.
    High,
}

impl From<VerbosityArg> for Verbosity {
    fn from(arg: VerbosityArg) -> Self {
        match arg {
            VerbosityArg::Low => Self::Low,
            VerbosityArg::Medium => Self::Medium,
            VerbosityArg::High => Self::High,
        }
    }
}

#[derive(Subcommand)]
enum ExclusionsCommand {
    /// List configured exclusion rules.
    List,
    /// Add an exclusion rule (at least one matcher is required).
    Add {
        /// Match a specific application name.
        #[arg(long)]
        app: Option<String>,
        /// Match a specific accessibility role (e.g. "statusbar").
        #[arg(long)]
        role: Option<String>,
        /// Match accessible names by regular expression.
        #[arg(long = "name-regex")]
        name_regex: Option<String>,
        /// What to do when the rule matches.
        #[arg(long, default_value = "suppress")]
        action: ActionArg,
    },
    /// Remove the rule numbered N (as shown by `list`).
    Remove {
        /// 1-based rule number from `intone exclusions list`.
        index: usize,
    },
    /// Print the path to the settings file.
    Path,
}

/// CLI mirror of [`intone_core::Action`], so the core stays free of any CLI dependency.
#[derive(Clone, Copy, ValueEnum)]
enum ActionArg {
    /// Do not announce at all.
    Suppress,
    /// Announce a shortened summary instead of the full content.
    Summarize,
    /// Announce, but without interrupting in-progress speech.
    LowerPriority,
}

impl From<ActionArg> for Action {
    fn from(arg: ActionArg) -> Self {
        match arg {
            ActionArg::Suppress => Self::Suppress,
            ActionArg::Summarize => Self::Summarize,
            ActionArg::LowerPriority => Self::LowerPriority,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Exclusions { command } => run_exclusions(command),
        Command::Config { command } => run_config(command),
        Command::Voices { command } => run_voices(command),
    }
}

/// Dispatch a `voices` subcommand: query speech-dispatcher (SSIP) for modules and voices.
fn run_voices(command: VoicesCommand) -> Result<()> {
    match command {
        VoicesCommand::List { language } => {
            let mut client = SsipBuilder::new().build().context(
                "connecting to speech-dispatcher (is it installed and running? \
                 try `spd-say hello` to start it, then retry)",
            )?;
            // SSIP is write-then-read: name the client, then read each LIST's reply in turn.
            client
                .set_client_name(ClientName::new("intone", "voices"))
                .context("naming SSIP client")?;
            client
                .check_client_name_set()
                .context("confirming SSIP client name")?;
            client
                .list_output_modules()
                .context("requesting output modules")?;
            let modules = match client.receive().context("reading output modules")? {
                Response::OutputModulesListSent(modules) => modules,
                _ => Vec::new(),
            };
            client
                .list_synthesis_voices()
                .context("requesting synthesis voices")?;
            let voices = client
                .receive_synthesis_voices()
                .context("reading synthesis voices")?
                .into_iter()
                .map(|voice| intone_cli::VoiceInfo {
                    name: voice.name,
                    language: voice.language,
                    dialect: voice.dialect,
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                intone_cli::format_voices(&modules, &voices, language.as_deref())
            );
        }
    }
    Ok(())
}

/// Dispatch a `config` subcommand: show or change general settings.
fn run_config(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => {
            let settings = Settings::load().context("loading settings")?;
            println!("{}", intone_cli::format_config(&settings));
        }
        ConfigCommand::Verbosity { level } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.verbosity = level.into();
            settings.save().context("saving settings")?;
            println!(
                "verbosity set to {}",
                intone_cli::verbosity_label(settings.verbosity)
            );
        }
        ConfigCommand::Braille { state } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.braille = state.into();
            settings.save().context("saving settings")?;
            println!("braille {}", if settings.braille { "on" } else { "off" });
        }
        ConfigCommand::Voice { name } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.speech.voice = intone_cli::optional_setting(&name);
            settings.save().context("saving settings")?;
            report_optional("voice", &settings.speech.voice);
        }
        ConfigCommand::Module { name } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.speech.output_module = intone_cli::optional_setting(&name);
            settings.save().context("saving settings")?;
            report_optional("output module", &settings.speech.output_module);
        }
        ConfigCommand::Language { tag } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.speech.language = intone_cli::optional_setting(&tag);
            settings.save().context("saving settings")?;
            report_optional("language", &settings.speech.language);
        }
        ConfigCommand::Rate { value } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.speech.rate = intone_cli::checked_level(value)?;
            settings.save().context("saving settings")?;
            println!("rate set to {}", settings.speech.rate);
        }
        ConfigCommand::Pitch { value } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.speech.pitch = intone_cli::checked_level(value)?;
            settings.save().context("saving settings")?;
            println!("pitch set to {}", settings.speech.pitch);
        }
        ConfigCommand::Volume { value } => {
            let mut settings = Settings::load().context("loading settings")?;
            settings.speech.volume = intone_cli::checked_level(value)?;
            settings.save().context("saving settings")?;
            println!("volume set to {}", settings.speech.volume);
        }
        ConfigCommand::Rotation { names } => {
            let mut settings = Settings::load().context("loading settings")?;
            let count = names.len();
            settings.speech.rotation = names;
            settings.save().context("saving settings")?;
            if count == 0 {
                println!("voice rotation cleared");
            } else {
                println!("voice rotation set ({count} voices)");
            }
        }
        ConfigCommand::VoiceLang { tag, voice } => {
            let mut settings = Settings::load().context("loading settings")?;
            if voice == "default" {
                if settings.speech.by_language.remove(&tag).is_some() {
                    println!("removed language voice for {tag}");
                } else {
                    println!("no language voice was set for {tag}");
                }
            } else {
                settings
                    .speech
                    .by_language
                    .insert(tag.clone(), voice.clone());
                println!("language {tag} → voice {voice}");
            }
            settings.save().context("saving settings")?;
        }
        ConfigCommand::VoiceContext { context, voice } => {
            let mut settings = Settings::load().context("loading settings")?;
            let key = context.key();
            if voice == "default" {
                if settings.speech.by_context.remove(key).is_some() {
                    println!("removed {key} voice");
                } else {
                    println!("no {key} voice was set");
                }
            } else {
                settings
                    .speech
                    .by_context
                    .insert(key.to_owned(), voice.clone());
                println!("{key} voice → {voice}");
            }
            settings.save().context("saving settings")?;
        }
    }
    Ok(())
}

/// Print the new value of an optional speech setting, showing `default` when it was cleared.
fn report_optional(label: &str, value: &Option<String>) {
    match value {
        Some(v) => println!("{label} set to {v}"),
        None => println!("{label} reset to engine default"),
    }
}

/// Dispatch an `exclusions` subcommand: load settings, mutate, persist, and report.
fn run_exclusions(command: ExclusionsCommand) -> Result<()> {
    match command {
        ExclusionsCommand::List => {
            let settings = Settings::load().context("loading settings")?;
            println!("{}", intone_cli::format_list(&settings));
        }
        ExclusionsCommand::Add {
            app,
            role,
            name_regex,
            action,
        } => {
            let mut settings = Settings::load().context("loading settings")?;
            let rule = ExclusionRule {
                app,
                role,
                name_regex,
                action: action.into(),
            };
            intone_cli::add_rule(&mut settings, rule)?;
            settings.save().context("saving settings")?;
            println!("added rule; {} now configured", settings.exclusions.len());
        }
        ExclusionsCommand::Remove { index } => {
            let mut settings = Settings::load().context("loading settings")?;
            let removed = intone_cli::remove_rule(&mut settings, index)?;
            settings.save().context("saving settings")?;
            println!(
                "removed rule #{index} ([{}])",
                intone_cli::action_label(removed.action)
            );
        }
        ExclusionsCommand::Path => {
            let path = intone_core::settings::config_file().context("locating config file")?;
            println!("{}", path.display());
        }
    }
    Ok(())
}
