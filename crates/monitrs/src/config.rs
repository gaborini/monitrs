//! Versioned TOML configuration.
//!
//! Four rules from §12 shape this module, and each is easy to get subtly wrong:
//!
//! * **No file is created on first launch.** monitrs is useful with no
//!   configuration at all; `config init` is the only thing that writes, and it
//!   never overwrites.
//! * **An invalid value points at the exact key**, not at a line number or a
//!   generic "invalid config". Every problem carries its dotted key path.
//! * **CLI values override file values.** That is a merge, which requires knowing
//!   which CLI options were actually *given* — hence `Option<T>` throughout
//!   [`crate::cli`].
//! * **Reload is atomic.** The whole candidate is parsed and validated before
//!   anything replaces the running configuration, so a typo cannot leave monitrs
//!   half-reconfigured.
//!
//! Configuration is data. §3.2 forbids executing anything from it, and §12 rules
//! out environment-variable interpolation in v1 — which is detected and reported
//! rather than silently ignored, because a user who writes `${HOME}` and gets a
//! literal `${HOME}` has been misled.

// `config path`, `config init`, and `config check` are wired up; the loading,
// merging, and reload API is complete and covered by the tests below but is not
// called yet, because its caller is the interactive runtime. Scoped to non-test
// builds so the tests still prove the API works rather than hiding it.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use monitrs_core::model::CpuNormalization;
use monitrs_core::units::{ByteUnits, format_duration, parse_bytes, parse_duration};
use monitrs_tui::action::SortField;
use monitrs_tui::event::{Key, KeyPress, Modifiers};
use monitrs_tui::glyphs::GlyphMode;
use monitrs_tui::theme::ColorMode;
use serde::{Deserialize, Serialize};

use crate::cli::{Cli, ColorModeArg, GlyphModeArg, SortFieldArg, UnitsArg};

/// The only configuration schema version this build understands.
pub(crate) const SUPPORTED_VERSION: u32 = 1;

/// One thing wrong with a configuration file, named by its key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigProblem {
    /// The dotted key path, e.g. `sampling.interval`.
    pub(crate) key: String,
    /// What is wrong with it, and what would be acceptable.
    pub(crate) message: String,
}

impl std::fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file we tried to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The file is not valid TOML, or has a field we do not know.
    ///
    /// `toml`'s own message already carries the line, column, and offending key,
    /// so it is passed through rather than reworded.
    #[error("{path} is not valid configuration:\n{message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message, plus a suggestion when one is obvious.
        message: String,
    },

    /// The file parsed but the values are out of range or contradictory.
    #[error("{} problem(s) in {}:\n{}", problems.len(), path.display(), format_problems(problems))]
    Invalid {
        /// The file.
        path: PathBuf,
        /// Every problem found, not just the first (§12).
        problems: Vec<ConfigProblem>,
    },

    /// The schema version is not one this build supports.
    #[error(
        "{path} declares config_version = {found}, but this build of monitrs supports \
         config_version = {SUPPORTED_VERSION}. {advice}"
    )]
    UnsupportedVersion {
        /// The file.
        path: PathBuf,
        /// What it declared.
        found: u32,
        /// What to do about it.
        advice: &'static str,
    },

    /// `config init` was asked to write over an existing file.
    ///
    /// A distinct variant rather than a dressed-up I/O error, so the message reads
    /// as the deliberate refusal §12 requires rather than as a failure.
    #[error(
        "{path} already exists; monitrs will not overwrite a configuration file. \
         Delete it, or pass a different path."
    )]
    AlreadyExists {
        /// The file that is in the way.
        path: PathBuf,
    },

    /// A `[keys]` binding is malformed or conflicts with another.
    #[error("{path}: {source}")]
    Keymap {
        /// The file.
        path: PathBuf,
        /// The keymap's own diagnosis, which names the key and both actions.
        source: monitrs_tui::keymap::KeymapError,
    },
}

fn format_problems(problems: &[ConfigProblem]) -> String {
    let mut out = String::new();
    for problem in problems {
        let _ = writeln!(out, "  {problem}");
    }
    out.trim_end().to_owned()
}

/// Where the running configuration came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigSource {
    /// `--config <path>`.
    Explicit(PathBuf),
    /// Found at the platform's user configuration path.
    Discovered(PathBuf),
    /// No file: built-in defaults, either because none exists or `--no-config`.
    Defaults,
}

impl ConfigSource {
    /// The file this came from, if any.
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Explicit(path) | Self::Discovered(path) => Some(path),
            Self::Defaults => None,
        }
    }
}

/// A loaded configuration together with anything worth telling the user.
#[derive(Debug)]
pub(crate) struct LoadedConfig {
    /// The effective configuration, before CLI overrides.
    pub(crate) config: Config,
    /// Where it came from.
    pub(crate) source: ConfigSource,
    /// Non-fatal notes: clamped values, unsupported constructs.
    ///
    /// §8.5 requires warning when a value was clamped, because silently doing
    /// something other than what was asked is worse than refusing.
    pub(crate) warnings: Vec<String>,
}

/// Sampling intervals and history retention (§8.5, §8.6).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SamplingConfig {
    /// Fast-tier interval.
    #[serde(with = "duration_text")]
    pub(crate) interval: Duration,
    /// How much history to retain.
    #[serde(with = "duration_text")]
    pub(crate) history: Duration,
    /// Medium-tier interval.
    #[serde(with = "duration_text")]
    pub(crate) medium_interval: Duration,
    /// Slow-tier interval.
    #[serde(with = "duration_text")]
    pub(crate) slow_interval: Duration,
    /// Memory budget for the history ring.
    #[serde(with = "bytes_text")]
    pub(crate) max_history_memory: u64,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            history: Duration::from_secs(300),
            medium_interval: Duration::from_secs(5),
            slow_interval: Duration::from_secs(30),
            max_history_memory: 32 * 1024 * 1024,
        }
    }
}

/// How the interface looks (§5).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct DisplayConfig {
    /// Glyph mode.
    pub(crate) glyphs: GlyphModeSetting,
    /// Colour depth.
    pub(crate) color: ColorModeSetting,
    /// Theme name.
    pub(crate) theme: String,
    /// Byte unit family.
    pub(crate) units: UnitsSetting,
    /// Whether process CPU is core- or machine-normalized (§8.3).
    pub(crate) process_cpu_normalization: NormalizationSetting,
    /// Whether to capture the mouse.
    pub(crate) mouse: bool,
    /// Whether the Overview shows per-core CPU.
    pub(crate) show_per_core: bool,
    /// Whether kernel threads are listed (Linux only).
    pub(crate) show_kernel_threads: bool,
    /// How the command column is sized.
    pub(crate) command_column: CommandColumnSetting,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            glyphs: GlyphModeSetting::Auto,
            color: ColorModeSetting::Auto,
            theme: "default-dark".to_owned(),
            units: UnitsSetting::Iec,
            process_cpu_normalization: NormalizationSetting::Core,
            mouse: false,
            show_per_core: false,
            show_kernel_threads: false,
            command_column: CommandColumnSetting::Auto,
        }
    }
}

/// Process table defaults (§7.2).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ProcessesConfig {
    /// Initial sort column.
    pub(crate) sort: String,
    /// Whether the initial sort is descending.
    pub(crate) descending: bool,
    /// Whether to start in tree mode.
    pub(crate) tree: bool,
    /// Initial filter text.
    pub(crate) filter: String,
    /// How many contributors per metric each history sample retains (§8.5).
    pub(crate) top_contributors_per_metric: u16,
}

impl Default for ProcessesConfig {
    fn default() -> Self {
        Self {
            sort: "cpu".to_owned(),
            descending: true,
            tree: false,
            filter: String::new(),
            top_contributors_per_metric: 10,
        }
    }
}

/// Diagnostic thresholds (§11.3, §12).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct DiagnosticsConfig {
    /// Whether the diagnostic engine runs at all.
    pub(crate) enabled: bool,
    /// CPU percentage at which the radar goes to `watch`.
    pub(crate) cpu_watch_percent: u8,
    /// CPU percentage at which the radar goes to `critical`.
    pub(crate) cpu_critical_percent: u8,
    /// Available-memory share below which the radar goes to `watch`.
    pub(crate) memory_watch_available_percent: u8,
    /// Available-memory share below which the radar goes to `critical`.
    pub(crate) memory_critical_available_percent: u8,
    /// How many of the recent samples must agree before escalating.
    pub(crate) sustained_samples: u16,
    /// Whether a signal escalating to `critical` also rings the terminal bell.
    ///
    /// Off by default. monitrs is frequently left running on a second screen, and a
    /// monitor that beeps without being asked to is a monitor that gets closed.
    pub(crate) bell_on_critical: bool,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cpu_watch_percent: 80,
            cpu_critical_percent: 95,
            memory_watch_available_percent: 15,
            memory_critical_available_percent: 5,
            sustained_samples: 10,
            bell_on_critical: false,
        }
    }
}

/// Key rebindings (§12 `[keys]`).
///
/// Only a documented subset is rebindable in v1. Every entry replaces the
/// built-in binding for that action; conflicts are rejected rather than resolved
/// by precedence (§21 M6).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct KeysConfig {
    /// Keys that quit.
    pub(crate) quit: Option<Vec<String>>,
    /// Keys that open help.
    pub(crate) help: Option<Vec<String>>,
    /// Keys that start filter editing.
    pub(crate) filter: Option<Vec<String>>,
    /// Keys that pause and resume the visible timeline.
    pub(crate) pause: Option<Vec<String>>,
    /// Keys that return to live.
    pub(crate) live: Option<Vec<String>>,
}

impl KeysConfig {
    /// Every configured binding as `(config key, action label, key strings)`.
    fn entries(&self) -> Vec<(&'static str, &'static str, &[String])> {
        let mut out: Vec<(&'static str, &'static str, &[String])> = Vec::new();
        for (key, label, value) in [
            ("keys.quit", "Quit", &self.quit),
            ("keys.help", "ToggleHelp", &self.help),
            ("keys.filter", "BeginFilterEdit", &self.filter),
            ("keys.pause", "TogglePause", &self.pause),
            ("keys.live", "ReturnLive", &self.live),
        ] {
            if let Some(keys) = value {
                out.push((key, label, keys.as_slice()));
            }
        }
        out
    }
}

/// The whole configuration file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Config {
    /// Schema version. Checked before anything else is trusted.
    pub(crate) config_version: u32,
    /// Sampling and history.
    pub(crate) sampling: SamplingConfig,
    /// Appearance.
    pub(crate) display: DisplayConfig,
    /// Process table.
    pub(crate) processes: ProcessesConfig,
    /// Diagnostics.
    pub(crate) diagnostics: DiagnosticsConfig,
    /// Key rebindings.
    pub(crate) keys: KeysConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: SUPPORTED_VERSION,
            sampling: SamplingConfig::default(),
            display: DisplayConfig::default(),
            processes: ProcessesConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            keys: KeysConfig::default(),
        }
    }
}

// --- value enums ------------------------------------------------------------
//
// These mirror the CLI value enums rather than reusing them, for the same reason
// the CLI ones do not reuse the library types: a configuration file is a stable
// user-facing surface whose spelling must not follow an internal refactor.

/// `display.glyphs`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GlyphModeSetting {
    /// Unicode when the locale supports it, strict ASCII otherwise.
    Auto,
    /// Enhanced Unicode.
    Unicode,
    /// Strict printable 7-bit ASCII.
    Ascii,
}

impl From<GlyphModeSetting> for GlyphMode {
    fn from(value: GlyphModeSetting) -> Self {
        match value {
            GlyphModeSetting::Auto => Self::Auto,
            GlyphModeSetting::Unicode => Self::Unicode,
            GlyphModeSetting::Ascii => Self::Ascii,
        }
    }
}

impl From<GlyphModeArg> for GlyphModeSetting {
    fn from(value: GlyphModeArg) -> Self {
        match value {
            GlyphModeArg::Auto => Self::Auto,
            GlyphModeArg::Unicode => Self::Unicode,
            GlyphModeArg::Ascii => Self::Ascii,
        }
    }
}

/// `display.color`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ColorModeSetting {
    /// Detect from the terminal, honouring `NO_COLOR`.
    Auto,
    /// 24-bit colour.
    Truecolor,
    /// 256-colour palette.
    #[serde(rename = "256")]
    Ansi256,
    /// 16-colour palette.
    #[serde(rename = "16")]
    Ansi16,
    /// No colour.
    Off,
}

impl From<ColorModeSetting> for ColorMode {
    fn from(value: ColorModeSetting) -> Self {
        match value {
            ColorModeSetting::Auto => Self::Auto,
            ColorModeSetting::Truecolor => Self::TrueColor,
            ColorModeSetting::Ansi256 => Self::Ansi256,
            ColorModeSetting::Ansi16 => Self::Ansi16,
            ColorModeSetting::Off => Self::Off,
        }
    }
}

impl From<ColorModeArg> for ColorModeSetting {
    fn from(value: ColorModeArg) -> Self {
        match value {
            ColorModeArg::Auto => Self::Auto,
            ColorModeArg::Truecolor => Self::Truecolor,
            ColorModeArg::Ansi256 => Self::Ansi256,
            ColorModeArg::Ansi16 => Self::Ansi16,
            ColorModeArg::Off => Self::Off,
        }
    }
}

/// `display.units`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UnitsSetting {
    /// Powers of 1024.
    Iec,
    /// Powers of 1000.
    Si,
}

impl From<UnitsSetting> for ByteUnits {
    fn from(value: UnitsSetting) -> Self {
        match value {
            UnitsSetting::Iec => Self::Iec,
            UnitsSetting::Si => Self::Si,
        }
    }
}

impl From<UnitsArg> for UnitsSetting {
    fn from(value: UnitsArg) -> Self {
        match value {
            UnitsArg::Iec => Self::Iec,
            UnitsArg::Si => Self::Si,
        }
    }
}

/// `display.process_cpu_normalization` (§8.3).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NormalizationSetting {
    /// One core = 100%. A multi-threaded process may exceed 100%.
    Core,
    /// The whole machine = 100%.
    Machine,
}

impl From<NormalizationSetting> for CpuNormalization {
    fn from(value: NormalizationSetting) -> Self {
        match value {
            NormalizationSetting::Core => Self::Core,
            NormalizationSetting::Machine => Self::Machine,
        }
    }
}

/// `display.command_column`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CommandColumnSetting {
    /// Width chosen from the available space.
    Auto,
    /// Just the process name.
    Name,
    /// The full command line.
    Full,
}

// --- serde helpers ----------------------------------------------------------

/// Durations are written as `"1s"`, `"250ms"`, `"5m"` — the grammar
/// [`parse_duration`] documents — so the file reads like the `--help` text.
mod duration_text {
    use super::{Duration, format_duration, parse_duration};
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_duration(*value))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

/// Byte sizes are written as `"32MiB"`, matching what monitrs displays.
mod bytes_text {
    use super::parse_bytes;
    use monitrs_core::units::{ByteUnits, format_bytes};
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_bytes(*value, ByteUnits::Iec).replace(' ', ""))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_bytes(&text).map_err(serde::de::Error::custom)
    }
}

// --- key parsing ------------------------------------------------------------

/// Parses a `[keys]` value such as `"q"`, `"ctrl-c"`, `"space"`, or `"shift-tab"`.
///
/// Modifier names are matched case-insensitively but a bare character is taken
/// **literally**, because §6.2 treats `g` and `G` as different keys and silently
/// lower-casing would quietly rebind the wrong one.
pub(crate) fn parse_key(text: &str) -> Result<KeyPress, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("an empty string is not a key".to_owned());
    }

    let mut modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
    };
    let mut remainder = trimmed;
    while let Some((prefix, rest)) = remainder.split_once('-') {
        if rest.is_empty() {
            // `"ctrl-"` or a literal `-`: stop and let the name lookup decide.
            break;
        }
        match prefix.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => modifiers.ctrl = true,
            "alt" | "option" | "meta" | "a" => modifiers.alt = true,
            "shift" | "s" => modifiers.shift = true,
            _ => break,
        }
        remainder = rest;
    }

    let key = match remainder.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" => Key::Enter,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "backtab" => Key::BackTab,
        "backspace" | "bs" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "left" => Key::Left,
        "right" => Key::Right,
        "up" => Key::Up,
        "down" => Key::Down,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "space" | "spc" => Key::Char(' '),
        other => {
            if let Some(number) = other.strip_prefix('f')
                && let Ok(index) = number.parse::<u8>()
            {
                if (1..=24).contains(&index) {
                    return Ok(KeyPress::new(Key::Function(index), modifiers));
                }
                return Err(format!("function key F{index} does not exist"));
            }
            // A single character, taken literally so case is preserved.
            let mut chars = remainder.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Char(c),
                _ => {
                    return Err(format!(
                        "{remainder:?} is not a key name; expected a single character or one of \
                         enter, esc, tab, backtab, backspace, delete, insert, space, \
                         left/right/up/down, home, end, pageup, pagedown, f1-f24"
                    ));
                }
            }
        }
    };

    // `shift-a` and `A` are the same key press; terminals report the shifted
    // character, so folding it here keeps one canonical form.
    if modifiers.shift
        && let Key::Char(c) = key
        && c.is_ascii_lowercase()
    {
        return Ok(KeyPress::new(
            Key::Char(c.to_ascii_uppercase()),
            Modifiers {
                shift: false,
                ..modifiers
            },
        ));
    }
    Ok(KeyPress::new(key, modifiers))
}

// --- discovery, loading, validation ----------------------------------------

/// The platform-appropriate configuration path, whether or not it exists.
///
/// Returns `None` only when the platform gives us no configuration directory at
/// all, which is a real state on a stripped-down container.
pub(crate) fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "monitrs")
        .map(|dirs| dirs.config_dir().join("monitrs.toml"))
}

/// Loads configuration following §12's search order.
///
/// An explicit `--config` path that does not exist is an **error**: the user
/// named a file, and silently falling back to defaults would hide a typo. A
/// *discovered* path that does not exist is normal and yields defaults.
pub(crate) fn load(explicit: Option<&Path>, no_config: bool) -> Result<LoadedConfig, ConfigError> {
    if no_config {
        return Ok(LoadedConfig {
            config: Config::default(),
            source: ConfigSource::Defaults,
            warnings: Vec::new(),
        });
    }

    if let Some(path) = explicit {
        let (config, warnings) = read_and_validate(path)?;
        return Ok(LoadedConfig {
            config,
            source: ConfigSource::Explicit(path.to_owned()),
            warnings,
        });
    }

    match default_path() {
        Some(path) if path.is_file() => {
            let (config, warnings) = read_and_validate(&path)?;
            Ok(LoadedConfig {
                config,
                source: ConfigSource::Discovered(path),
                warnings,
            })
        }
        _ => Ok(LoadedConfig {
            config: Config::default(),
            source: ConfigSource::Defaults,
            warnings: Vec::new(),
        }),
    }
}

/// Reads, parses, and fully validates one file.
///
/// Every step must succeed before the configuration is returned, which is what
/// makes [`reload`] atomic: a caller never sees a partially valid candidate.
pub(crate) fn read_and_validate(path: &Path) -> Result<(Config, Vec<String>), ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;

    let mut warnings = Vec::new();
    if let Some(line) = text.lines().find(|line| line.contains("${")) {
        // §12: no environment interpolation in v1. Saying so is much kinder than
        // letting a literal `${HOME}` reach a path.
        warnings.push(format!(
            "environment variable interpolation is not supported in v1; the value in \
             {line:?} is used literally"
        ));
    }

    // The version is read before the rest is trusted, so a future schema produces
    // a clear message instead of a pile of unknown-field errors.
    if let Some(found) = peek_version(&text)
        && found != SUPPORTED_VERSION
    {
        return Err(ConfigError::UnsupportedVersion {
            path: path.to_owned(),
            found,
            advice: if found > SUPPORTED_VERSION {
                "the file was written by a newer monitrs; upgrade monitrs or remove the file."
            } else {
                "run `monitrs config init` against a new path to see the current format."
            },
        });
    }

    let config: Config = toml::from_str(&text).map_err(|error| ConfigError::Parse {
        path: path.to_owned(),
        message: enrich_parse_error(&error.to_string()),
    })?;

    let problems = config.validate();
    if !problems.is_empty() {
        return Err(ConfigError::Invalid {
            path: path.to_owned(),
            problems,
        });
    }
    config
        .validate_keys()
        .map_err(|source| ConfigError::Keymap {
            path: path.to_owned(),
            source,
        })?;

    Ok((config, warnings))
}

/// Reads only `config_version`, tolerating everything else being unknown.
fn peek_version(text: &str) -> Option<u32> {
    #[derive(Deserialize)]
    struct VersionOnly {
        config_version: Option<u32>,
    }
    toml::from_str::<VersionOnly>(text).ok()?.config_version
}

/// Appends a "did you mean" suggestion to an unknown-field error.
///
/// §12 asks for an explicit unknown-field policy. Ours is to reject — a silently
/// ignored key is a setting the user believes is in effect — and to make the
/// rejection actionable.
fn enrich_parse_error(message: &str) -> String {
    let Some(unknown) = message
        .split_once("unknown field `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(field, _)| field)
    else {
        return message.to_owned();
    };
    match closest_key(unknown) {
        Some(suggestion) => format!("{message}\n\ndid you mean `{suggestion}`?"),
        None => message.to_owned(),
    }
}

/// Every key name a configuration file may contain.
fn known_keys() -> BTreeSet<&'static str> {
    [
        "config_version",
        "sampling",
        "interval",
        "history",
        "medium_interval",
        "slow_interval",
        "max_history_memory",
        "display",
        "glyphs",
        "color",
        "theme",
        "units",
        "process_cpu_normalization",
        "mouse",
        "show_per_core",
        "show_kernel_threads",
        "command_column",
        "processes",
        "sort",
        "descending",
        "tree",
        "filter",
        "top_contributors_per_metric",
        "diagnostics",
        "enabled",
        "cpu_watch_percent",
        "cpu_critical_percent",
        "memory_watch_available_percent",
        "memory_critical_available_percent",
        "sustained_samples",
        "bell_on_critical",
        "keys",
        "quit",
        "help",
        "pause",
        "live",
    ]
    .into_iter()
    .collect()
}

/// The known key within edit distance 2 of `unknown`, if any.
fn closest_key(unknown: &str) -> Option<&'static str> {
    known_keys()
        .into_iter()
        .map(|candidate| (edit_distance(unknown, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, iterative and allocation-light.
fn edit_distance(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0usize; right_chars.len() + 1];

    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, &right_char) in right_chars.iter().enumerate() {
            let substitution = previous
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(usize::from(left_char != right_char));
            let deletion = previous
                .get(j + 1)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let insertion = current
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            if let Some(slot) = current.get_mut(j + 1) {
                *slot = substitution.min(deletion).min(insertion);
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous.last().copied().unwrap_or(0)
}

impl Config {
    /// Every range and consistency problem, each naming its exact key (§12).
    ///
    /// Returns all of them rather than the first, so a user fixing three mistakes
    /// does not have to run monitrs three times.
    pub(crate) fn validate(&self) -> Vec<ConfigProblem> {
        let mut problems = Vec::new();
        let mut problem = |key: &str, message: String| {
            problems.push(ConfigProblem {
                key: key.to_owned(),
                message,
            });
        };

        if self.config_version != SUPPORTED_VERSION {
            problem(
                "config_version",
                format!("must be {SUPPORTED_VERSION}, got {}", self.config_version),
            );
        }

        let interval = self.sampling.interval;
        if !(crate::cli::MIN_INTERVAL..=crate::cli::MAX_INTERVAL).contains(&interval) {
            problem(
                "sampling.interval",
                format!(
                    "must be between {} and {}, got {}",
                    format_duration(crate::cli::MIN_INTERVAL),
                    format_duration(crate::cli::MAX_INTERVAL),
                    format_duration(interval)
                ),
            );
        }
        let history = self.sampling.history;
        if !(crate::cli::MIN_HISTORY..=crate::cli::MAX_HISTORY).contains(&history) {
            problem(
                "sampling.history",
                format!(
                    "must be between {} and {}, got {}",
                    format_duration(crate::cli::MIN_HISTORY),
                    format_duration(crate::cli::MAX_HISTORY),
                    format_duration(history)
                ),
            );
        }
        if history < interval {
            problem(
                "sampling.history",
                format!(
                    "must be at least one sampling.interval ({}), got {}",
                    format_duration(interval),
                    format_duration(history)
                ),
            );
        }
        for (key, value) in [
            ("sampling.medium_interval", self.sampling.medium_interval),
            ("sampling.slow_interval", self.sampling.slow_interval),
        ] {
            if value < interval {
                problem(
                    key,
                    format!(
                        "must not be shorter than sampling.interval ({}), got {}",
                        format_duration(interval),
                        format_duration(value)
                    ),
                );
            }
        }
        // 1 MiB holds roughly a thousand compact samples; below that the ring
        // could not honour even the 30s minimum history.
        if self.sampling.max_history_memory < 1024 * 1024 {
            problem(
                "sampling.max_history_memory",
                format!(
                    "must be at least 1MiB, got {}",
                    monitrs_core::units::format_bytes(
                        self.sampling.max_history_memory,
                        ByteUnits::Iec
                    )
                ),
            );
        }

        if self.processes.top_contributors_per_metric == 0
            || self.processes.top_contributors_per_metric > 100
        {
            problem(
                "processes.top_contributors_per_metric",
                format!(
                    "must be between 1 and 100, got {}",
                    self.processes.top_contributors_per_metric
                ),
            );
        }
        if SortField::from_token(&self.processes.sort).is_none() {
            problem(
                "processes.sort",
                format!(
                    "{:?} is not a sortable column; expected one of cpu, memory, read, write, \
                     pid, name, age, user, state, threads, virtual",
                    self.processes.sort
                ),
            );
        }

        for (key, value) in [
            (
                "diagnostics.cpu_watch_percent",
                self.diagnostics.cpu_watch_percent,
            ),
            (
                "diagnostics.cpu_critical_percent",
                self.diagnostics.cpu_critical_percent,
            ),
            (
                "diagnostics.memory_watch_available_percent",
                self.diagnostics.memory_watch_available_percent,
            ),
            (
                "diagnostics.memory_critical_available_percent",
                self.diagnostics.memory_critical_available_percent,
            ),
        ] {
            if value > 100 {
                problem(key, format!("must be a percentage in 0..=100, got {value}"));
            }
        }
        if self.diagnostics.cpu_watch_percent >= self.diagnostics.cpu_critical_percent {
            problem(
                "diagnostics.cpu_watch_percent",
                format!(
                    "must be below diagnostics.cpu_critical_percent ({}), got {}",
                    self.diagnostics.cpu_critical_percent, self.diagnostics.cpu_watch_percent
                ),
            );
        }
        // Available-memory thresholds run the other way: less available is worse.
        if self.diagnostics.memory_critical_available_percent
            >= self.diagnostics.memory_watch_available_percent
        {
            problem(
                "diagnostics.memory_critical_available_percent",
                format!(
                    "must be below diagnostics.memory_watch_available_percent ({}), got {}",
                    self.diagnostics.memory_watch_available_percent,
                    self.diagnostics.memory_critical_available_percent
                ),
            );
        }
        if self.diagnostics.sustained_samples == 0 || self.diagnostics.sustained_samples > 600 {
            problem(
                "diagnostics.sustained_samples",
                format!(
                    "must be between 1 and 600, got {}",
                    self.diagnostics.sustained_samples
                ),
            );
        }

        // A bool has no range to check, but it can still contradict another key: with
        // diagnostics off no signal ever escalates, so a bell asked for here could
        // never ring. §12 requires that to be named rather than silently ignored.
        if self.diagnostics.bell_on_critical && !self.diagnostics.enabled {
            problem(
                "diagnostics.bell_on_critical",
                "cannot ring while diagnostics.enabled is false, because no pressure \
                 signal is derived at all"
                    .to_owned(),
            );
        }

        if self.display.theme.trim().is_empty() {
            problem("display.theme", "must not be empty".to_owned());
        }

        for (key, _, values) in self.keys.entries() {
            if values.is_empty() {
                problem(key, "must list at least one key".to_owned());
                continue;
            }
            for value in values {
                if let Err(reason) = parse_key(value) {
                    problem(key, reason);
                }
            }
        }

        problems
    }

    /// Rejects `[keys]` tables that bind one key to two actions (§12, §21 M6).
    pub(crate) fn validate_keys(&self) -> Result<(), monitrs_tui::keymap::KeymapError> {
        let mut seen: Vec<(KeyPress, &'static str)> = Vec::new();
        for (_, action, values) in self.keys.entries() {
            for value in values {
                let Ok(press) = parse_key(value) else {
                    continue;
                };
                if let Some((_, other)) = seen
                    .iter()
                    .find(|(existing, other)| *existing == press && *other != action)
                {
                    return Err(monitrs_tui::keymap::KeymapError::Conflict {
                        mode: "Normal",
                        key: press.label(),
                        first: (*other).to_owned(),
                        second: action.to_owned(),
                    });
                }
                seen.push((press, action));
            }
        }
        Ok(())
    }

    /// Applies CLI overrides. CLI always wins (§12).
    pub(crate) fn apply_cli(&mut self, cli: &Cli) {
        if let Some(glyphs) = cli.glyph_mode() {
            self.display.glyphs = glyphs.into();
        }
        if let Some(color) = cli.color_mode() {
            self.display.color = color.into();
        }
        if let Some(theme) = &cli.display.theme {
            self.display.theme = theme.clone();
        }
        if let Some(units) = cli.display.units {
            self.display.units = units.into();
        }
        if cli.display.mouse {
            self.display.mouse = true;
        }
        if let Some(interval) = cli.sampling.interval {
            self.sampling.interval = interval;
        }
        if let Some(history) = cli.sampling.history {
            self.sampling.history = history;
        }
        if cli.view.tree {
            self.processes.tree = true;
        }
        if cli.view.per_core {
            self.display.show_per_core = true;
        }
        if let Some(filter) = &cli.view.filter {
            self.processes.filter = filter.clone();
        }
        if let Some(sort) = cli.view.sort {
            self.processes.sort = sort_token(sort).to_owned();
        }
    }

    /// Settings that cannot take effect without restarting (§12).
    ///
    /// Reported to the user rather than silently ignored, because a reload that
    /// appears to succeed but changed nothing is indistinguishable from a bug.
    pub(crate) fn non_reloadable_changes(&self, candidate: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.display.mouse != candidate.display.mouse {
            // Mouse capture is a terminal mode set once by the guard (§14.3).
            changed.push("display.mouse");
        }
        if self.config_version != candidate.config_version {
            changed.push("config_version");
        }
        changed
    }
}

fn sort_token(sort: SortFieldArg) -> &'static str {
    match sort {
        SortFieldArg::Cpu => "cpu",
        SortFieldArg::Memory => "memory",
        SortFieldArg::Read => "read",
        SortFieldArg::Write => "write",
        SortFieldArg::Pid => "pid",
        SortFieldArg::Name => "name",
        SortFieldArg::Age => "age",
        SortFieldArg::User => "user",
        SortFieldArg::State => "state",
        SortFieldArg::Threads => "threads",
    }
}

/// The result of an atomic reload.
#[derive(Debug)]
pub(crate) struct ReloadOutcome {
    /// The validated replacement.
    pub(crate) config: Config,
    /// Settings that changed but cannot take effect until restart.
    pub(crate) non_reloadable: Vec<&'static str>,
    /// Non-fatal notes from parsing.
    pub(crate) warnings: Vec<String>,
}

/// Validates a candidate file against the running configuration.
///
/// Returns `Err` **without touching** `current` if anything is wrong, which is
/// what §12 means by an atomic reload.
pub(crate) fn reload(current: &Config, path: &Path) -> Result<ReloadOutcome, ConfigError> {
    let (candidate, warnings) = read_and_validate(path)?;
    let non_reloadable = current.non_reloadable_changes(&candidate);
    Ok(ReloadOutcome {
        config: candidate,
        non_reloadable,
        warnings,
    })
}

/// The documented starter file `monitrs config init` writes.
///
/// Every value shown is the built-in default, so an untouched file changes
/// nothing — which makes it safe to write and easy to experiment with.
pub(crate) fn starter_file() -> String {
    let defaults = Config::default();
    format!(
        r#"# monitrs configuration.
#
# Every value here is the built-in default, so this file changes nothing until
# you edit it. Delete any key to fall back to the default.
#
# Validate without launching:  monitrs config check
# Show this path:              monitrs config path
#
# Durations are written as 250ms, 1s, 5m, 1h. Sizes as 512kB, 32MiB, 1.5GiB.

config_version = {version}

[sampling]
# How often the fast tier samples CPU, memory, processes, and counters.
# Range: 250ms to 60s.
interval = "{interval}"
# How much history Time Lens retains. Range: 30s to 1h.
history = "{history}"
# Filesystem capacity, device state, and sensors.
medium_interval = "{medium}"
# Users, device lists, and static metadata.
slow_interval = "{slow}"
# Ceiling for the history ring. If interval and history would need more than
# this, history is shortened and monitrs tells you it was clamped.
max_history_memory = "{memory}"

[display]
# auto | unicode | ascii. `auto` uses Unicode on a UTF-8 locale and falls back
# to strict 7-bit ASCII otherwise.
glyphs = "auto"
# auto | truecolor | 256 | 16 | off. `auto` honours the NO_COLOR convention.
color = "auto"
# default-dark | default-light | high-contrast
theme = "{theme}"
# iec (KiB, MiB, GiB) | si (kB, MB, GB)
units = "iec"
# core    = one core is 100%, so a multi-threaded process may read 287%
# machine = the whole machine is 100%, so nothing exceeds 100%
process_cpu_normalization = "core"
mouse = {mouse}
show_per_core = {per_core}
# Linux only: kernel threads are hidden by default.
show_kernel_threads = {kernel_threads}
# auto | name | full
command_column = "auto"

[processes]
# cpu | memory | read | write | pid | name | age | user | state | threads | virtual
sort = "{sort}"
descending = {descending}
tree = {tree}
filter = ""
# How many contributors per metric each history sample keeps, for spike
# attribution. Range: 1 to 100. Higher means more memory per sample.
top_contributors_per_metric = {contributors}

[diagnostics]
enabled = {diagnostics_enabled}
cpu_watch_percent = {cpu_watch}
cpu_critical_percent = {cpu_critical}
# These run the other way round: *less* available memory is worse, so
# critical must be below watch.
memory_watch_available_percent = {mem_watch}
memory_critical_available_percent = {mem_critical}
# How many of the recent samples must agree before a signal escalates. This is
# the hysteresis that stops the radar flapping once per second.
sustained_samples = {sustained}
# A signal crossing into watch or critical is always recorded as a notice. Set
# this to also ring the terminal bell, once, when one reaches *critical* — never
# for watch, and never when it recovers. Off by default: a monitor left on a
# second screen should not beep unless you asked it to.
bell_on_critical = {bell}

# Key rebinding. Each entry replaces the built-in binding for that action.
# Binding one key to two actions is rejected, not silently resolved.
# Names: a single character (case-sensitive), or enter, esc, tab, backtab,
# backspace, delete, insert, space, left, right, up, down, home, end, pageup,
# pagedown, f1-f24. Prefix with ctrl-, alt-, or shift-.
#
# [keys]
# quit = ["q", "ctrl-c"]
# help = ["?"]
# filter = ["/"]
# pause = ["space"]
# live = ["L"]
"#,
        version = SUPPORTED_VERSION,
        interval = format_duration(defaults.sampling.interval),
        history = format_duration(defaults.sampling.history),
        medium = format_duration(defaults.sampling.medium_interval),
        slow = format_duration(defaults.sampling.slow_interval),
        memory =
            monitrs_core::units::format_bytes(defaults.sampling.max_history_memory, ByteUnits::Iec)
                .replace(' ', ""),
        theme = defaults.display.theme,
        mouse = defaults.display.mouse,
        per_core = defaults.display.show_per_core,
        kernel_threads = defaults.display.show_kernel_threads,
        sort = defaults.processes.sort,
        descending = defaults.processes.descending,
        tree = defaults.processes.tree,
        contributors = defaults.processes.top_contributors_per_metric,
        diagnostics_enabled = defaults.diagnostics.enabled,
        cpu_watch = defaults.diagnostics.cpu_watch_percent,
        cpu_critical = defaults.diagnostics.cpu_critical_percent,
        mem_watch = defaults.diagnostics.memory_watch_available_percent,
        mem_critical = defaults.diagnostics.memory_critical_available_percent,
        sustained = defaults.diagnostics.sustained_samples,
        bell = defaults.diagnostics.bell_on_critical,
    )
}

/// Writes the starter file, refusing to overwrite (§12, §21 M6).
pub(crate) fn init_file(path: &Path) -> Result<(), ConfigError> {
    if path.exists() {
        return Err(ConfigError::AlreadyExists {
            path: path.to_owned(),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Read {
            path: parent.to_owned(),
            source,
        })?;
    }
    std::fs::write(path, starter_file()).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    restrict_to_user(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })
}

/// Configuration uses user-only permissions where the platform supports it (§15.2).
#[cfg(unix)]
fn restrict_to_user(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_to_user(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory that removes itself, so no test leaves files behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "monitrs-config-test-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("create temp dir");
            Self(base)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.file(name);
            std::fs::write(&path, contents).expect("write fixture");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn problems_for(toml_text: &str) -> Vec<ConfigProblem> {
        toml::from_str::<Config>(toml_text)
            .expect("parses")
            .validate()
    }

    #[test]
    fn the_defaults_match_the_specification_and_are_valid() {
        let config = Config::default();
        assert_eq!(config.config_version, 1);
        assert_eq!(config.sampling.interval, Duration::from_secs(1));
        assert_eq!(config.sampling.history, Duration::from_secs(300));
        assert_eq!(config.sampling.medium_interval, Duration::from_secs(5));
        assert_eq!(config.sampling.slow_interval, Duration::from_secs(30));
        assert_eq!(config.sampling.max_history_memory, 32 * 1024 * 1024);
        assert_eq!(config.display.theme, "default-dark");
        assert_eq!(
            config.display.process_cpu_normalization,
            NormalizationSetting::Core
        );
        assert!(!config.display.mouse);
        assert_eq!(config.processes.sort, "cpu");
        assert!(config.processes.descending);
        assert_eq!(config.processes.top_contributors_per_metric, 10);
        assert!(config.diagnostics.enabled);
        assert_eq!(config.diagnostics.cpu_watch_percent, 80);
        assert_eq!(config.diagnostics.cpu_critical_percent, 95);
        assert_eq!(config.diagnostics.memory_watch_available_percent, 15);
        assert_eq!(config.diagnostics.memory_critical_available_percent, 5);
        assert_eq!(config.diagnostics.sustained_samples, 10);
        assert!(config.validate().is_empty(), "{:?}", config.validate());
    }

    #[test]
    fn an_empty_file_yields_the_defaults() {
        // Every field has a serde default, so a user can set one key and leave
        // the rest alone.
        let config: Config = toml::from_str("").expect("empty is valid");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn a_partial_file_changes_only_what_it_names() {
        let config: Config = toml::from_str(
            r#"
            config_version = 1
            [display]
            theme = "high-contrast"
            "#,
        )
        .expect("parses");
        assert_eq!(config.display.theme, "high-contrast");
        assert_eq!(config.sampling, SamplingConfig::default());
        assert_eq!(config.processes, ProcessesConfig::default());
    }

    #[test]
    fn the_starter_file_is_parseable_and_equals_the_defaults() {
        // §12: `config init` writes a documented starter file. If it did not
        // round-trip to the defaults, the comments would be lying.
        let config: Config = toml::from_str(&starter_file()).expect("starter file parses");
        assert_eq!(config, Config::default());
        assert!(config.validate().is_empty());
    }

    #[test]
    fn the_starter_file_documents_every_section() {
        let text = starter_file();
        for section in ["[sampling]", "[display]", "[processes]", "[diagnostics]"] {
            assert!(text.contains(section), "starter file omits {section}");
        }
        // The keys table is commented out on purpose: an active empty table would
        // be indistinguishable from an intentional rebinding.
        assert!(text.contains("# [keys]"));
        assert!(text.contains("config_version = 1"));
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_suggestion() {
        // §12's explicit policy: reject, because a silently ignored key is a
        // setting the user believes is in effect.
        let error = toml::from_str::<Config>(
            r#"
            config_version = 1
            [sampling]
            intervall = "1s"
            "#,
        )
        .expect_err("unknown field must be rejected");
        let enriched = enrich_parse_error(&error.to_string());
        assert!(enriched.contains("intervall"), "{enriched}");
        assert!(enriched.contains("did you mean `interval`?"), "{enriched}");
    }

    #[test]
    fn an_unrecognisable_key_is_rejected_without_a_bogus_suggestion() {
        let enriched = enrich_parse_error("unknown field `xyzzy_completely_unrelated`");
        assert!(!enriched.contains("did you mean"), "{enriched}");
    }

    #[test]
    fn edit_distance_is_symmetric_and_zero_for_equal_strings() {
        assert_eq!(edit_distance("interval", "interval"), 0);
        assert_eq!(edit_distance("intervall", "interval"), 1);
        assert_eq!(edit_distance("interval", "intervall"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("theme", "them"), 1);
    }

    #[test]
    fn an_invalid_duration_names_the_exact_key() {
        let error = toml::from_str::<Config>(
            r#"
            [sampling]
            interval = "1"
            "#,
        )
        .expect_err("a bare number is ambiguous");
        let message = error.to_string();
        assert!(message.contains("interval"), "{message}");
    }

    #[test]
    fn out_of_range_values_are_reported_against_their_own_keys() {
        let problems = problems_for(
            r#"
            [sampling]
            interval = "100ms"
            history = "10s"
            "#,
        );
        let keys: Vec<&str> = problems.iter().map(|p| p.key.as_str()).collect();
        assert!(keys.contains(&"sampling.interval"), "{problems:?}");
        assert!(keys.contains(&"sampling.history"), "{problems:?}");
    }

    #[test]
    fn every_problem_is_reported_not_only_the_first() {
        let problems = problems_for(
            r#"
            [sampling]
            interval = "100ms"
            history = "10s"
            [processes]
            sort = "entropy"
            top_contributors_per_metric = 0
            [diagnostics]
            sustained_samples = 0
            "#,
        );
        assert!(
            problems.len() >= 5,
            "expected several problems, got {problems:?}"
        );
    }

    #[test]
    fn a_secondary_tier_faster_than_the_fast_tier_is_rejected() {
        // Refreshing filesystem capacity more often than CPU is pointless work
        // and almost certainly a typo.
        let problems = problems_for(
            r#"
            [sampling]
            interval = "5s"
            medium_interval = "1s"
            "#,
        );
        assert!(
            problems.iter().any(|p| p.key == "sampling.medium_interval"),
            "{problems:?}"
        );
    }

    #[test]
    fn history_shorter_than_one_interval_is_rejected() {
        let problems = problems_for(
            r#"
            [sampling]
            interval = "60s"
            history = "30s"
            "#,
        );
        assert!(
            problems.iter().any(|p| p.key == "sampling.history"),
            "{problems:?}"
        );
    }

    #[test]
    fn cpu_thresholds_must_be_ordered() {
        let problems = problems_for(
            r#"
            [diagnostics]
            cpu_watch_percent = 95
            cpu_critical_percent = 80
            "#,
        );
        assert!(
            problems
                .iter()
                .any(|p| p.key == "diagnostics.cpu_watch_percent"),
            "{problems:?}"
        );
    }

    #[test]
    fn available_memory_thresholds_are_ordered_the_other_way_round() {
        // Less available memory is worse, so critical must be *below* watch.
        // Getting this backwards is the easiest mistake in the whole file.
        assert!(
            problems_for(
                r#"
            [diagnostics]
            memory_watch_available_percent = 15
            memory_critical_available_percent = 5
            "#
            )
            .is_empty()
        );

        let problems = problems_for(
            r#"
            [diagnostics]
            memory_watch_available_percent = 5
            memory_critical_available_percent = 15
            "#,
        );
        assert!(
            problems
                .iter()
                .any(|p| p.key == "diagnostics.memory_critical_available_percent"),
            "{problems:?}"
        );
    }

    #[test]
    fn the_bell_is_off_by_default_and_only_rings_for_critical() {
        // §12: the default has to be the quiet one. A monitor that beeps on a fresh
        // install is a monitor that gets uninstalled, and the notice is the primary
        // cue — the bell is the addition (§5.2's principle applied to sound).
        assert!(!Config::default().diagnostics.bell_on_critical);
        assert!(starter_file().contains("bell_on_critical = false"));
    }

    #[test]
    fn a_bell_that_could_never_ring_is_reported_rather_than_ignored() {
        // With diagnostics off no signal is derived at all, so this pair of keys
        // contradict each other. §12 wants that named, not silently dropped.
        let problems = problems_for(
            r"
            [diagnostics]
            enabled = false
            bell_on_critical = true
            ",
        );
        let problem = problems
            .iter()
            .find(|problem| problem.key == "diagnostics.bell_on_critical")
            .expect("the contradiction must be reported");
        assert!(problem.message.contains("diagnostics.enabled"), "{problem}");

        // Either key on its own is fine.
        assert!(
            problems_for("[diagnostics]\nbell_on_critical = true\n").is_empty(),
            "asking for the bell with diagnostics on is the point of the key"
        );
        assert!(problems_for("[diagnostics]\nenabled = false\n").is_empty());
    }

    #[test]
    fn an_unknown_sort_column_is_rejected_and_named() {
        let problems = problems_for(
            r#"
            [processes]
            sort = "entropy"
            "#,
        );
        let problem = problems
            .iter()
            .find(|p| p.key == "processes.sort")
            .expect("sort must be validated");
        assert!(problem.message.contains("entropy"), "{problem}");
        assert!(
            problem.message.contains("cpu"),
            "the message should list valid columns"
        );
    }

    #[test]
    fn every_specified_sort_column_is_accepted() {
        for column in [
            "cpu", "memory", "read", "write", "pid", "name", "age", "user", "state", "threads",
        ] {
            let problems = problems_for(&format!("[processes]\nsort = \"{column}\"\n"));
            assert!(problems.is_empty(), "{column} rejected: {problems:?}");
        }
    }

    #[test]
    fn byte_sizes_accept_both_families_and_a_fraction() {
        let config: Config = toml::from_str(
            r#"
            [sampling]
            max_history_memory = "1.5GiB"
            "#,
        )
        .expect("parses");
        assert_eq!(config.sampling.max_history_memory, 1_610_612_736);
        assert!(config.validate().is_empty());
    }

    #[test]
    fn an_absurdly_small_memory_budget_is_rejected() {
        let problems = problems_for(
            r#"
            [sampling]
            max_history_memory = "64kB"
            "#,
        );
        assert!(
            problems
                .iter()
                .any(|p| p.key == "sampling.max_history_memory"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_config_serializes_back_to_a_parseable_file() {
        let config = Config::default();
        let text = toml::to_string(&config).expect("serializes");
        let round_tripped: Config = toml::from_str(&text).expect("re-parses");
        assert_eq!(config, round_tripped);
        // Durations must round-trip as their text form, not as a table.
        assert!(text.contains("interval = \"1s\""), "{text}");
        assert!(text.contains("max_history_memory = \"32MiB\""), "{text}");
    }

    // --- key parsing --------------------------------------------------------

    #[test]
    fn plain_keys_parse() {
        assert_eq!(parse_key("q"), Ok(KeyPress::char('q')));
        assert_eq!(parse_key("?"), Ok(KeyPress::char('?')));
        assert_eq!(parse_key("/"), Ok(KeyPress::char('/')));
        assert_eq!(parse_key("space"), Ok(KeyPress::char(' ')));
        assert_eq!(parse_key("enter"), Ok(KeyPress::plain(Key::Enter)));
        assert_eq!(parse_key("esc"), Ok(KeyPress::plain(Key::Escape)));
        assert_eq!(parse_key("pageup"), Ok(KeyPress::plain(Key::PageUp)));
        assert_eq!(parse_key("f5"), Ok(KeyPress::plain(Key::Function(5))));
    }

    #[test]
    fn a_bare_character_keeps_its_case_so_upper_and_lower_stay_distinct() {
        // §6.2 binds `g` and `G` to different actions; lower-casing here would
        // silently rebind the wrong one.
        assert_eq!(parse_key("g"), Ok(KeyPress::char('g')));
        assert_eq!(parse_key("G"), Ok(KeyPress::char('G')));
        assert_ne!(parse_key("g"), parse_key("G"));
        assert_eq!(parse_key("L"), Ok(KeyPress::char('L')));
    }

    #[test]
    fn modifiers_parse_and_are_case_insensitive() {
        assert_eq!(parse_key("ctrl-c"), Ok(KeyPress::ctrl('c')));
        assert_eq!(parse_key("CTRL-c"), Ok(KeyPress::ctrl('c')));
        assert_eq!(parse_key("Control-c"), Ok(KeyPress::ctrl('c')));
        assert_eq!(
            parse_key("alt-x"),
            Ok(KeyPress::new(
                Key::Char('x'),
                Modifiers {
                    ctrl: false,
                    alt: true,
                    shift: false
                }
            ))
        );
    }

    #[test]
    fn shift_plus_a_letter_folds_to_the_shifted_character() {
        // Terminals report the shifted character, so `shift-a` and `A` must be
        // the same key press or a rebinding would never match.
        assert_eq!(parse_key("shift-a"), parse_key("A"));
        assert_eq!(
            parse_key("shift-tab"),
            Ok(KeyPress::new(
                Key::Tab,
                Modifiers {
                    ctrl: false,
                    alt: false,
                    shift: true
                }
            ))
        );
    }

    #[test]
    fn a_nonexistent_key_name_is_rejected_with_the_valid_names() {
        let error = parse_key("wiggle").expect_err("not a key");
        assert!(error.contains("wiggle"), "{error}");
        assert!(
            error.contains("pageup"),
            "the error should list valid names: {error}"
        );
        assert!(parse_key("").is_err());
        assert!(parse_key("f99").is_err());
    }

    #[test]
    fn an_invalid_key_binding_names_the_config_key() {
        let problems = problems_for(
            r#"
            [keys]
            quit = ["wiggle"]
            "#,
        );
        assert!(
            problems.iter().any(|p| p.key == "keys.quit"),
            "{problems:?}"
        );
    }

    #[test]
    fn an_empty_key_list_is_rejected() {
        let problems = problems_for(
            r#"
            [keys]
            quit = []
            "#,
        );
        assert!(
            problems.iter().any(|p| p.key == "keys.quit"),
            "{problems:?}"
        );
    }

    #[test]
    fn one_key_bound_to_two_actions_is_rejected_not_silently_resolved() {
        // §21 M6: key conflicts are *rejected*.
        let config: Config = toml::from_str(
            r#"
            [keys]
            quit = ["x"]
            help = ["x"]
            "#,
        )
        .expect("parses");
        let error = config
            .validate_keys()
            .expect_err("a conflict must be rejected");
        let message = error.to_string();
        assert!(message.contains("Quit"), "{message}");
        assert!(message.contains("ToggleHelp"), "{message}");
    }

    #[test]
    fn the_same_key_listed_twice_for_one_action_is_not_a_conflict() {
        let config: Config = toml::from_str(
            r#"
            [keys]
            quit = ["q", "q", "ctrl-c"]
            "#,
        )
        .expect("parses");
        assert!(config.validate_keys().is_ok());
    }

    // --- loading ------------------------------------------------------------

    #[test]
    fn no_config_uses_defaults_and_reads_nothing() {
        let loaded = load(None, true).expect("defaults are always available");
        assert_eq!(loaded.source, ConfigSource::Defaults);
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.source.path().is_none());
    }

    #[test]
    fn an_explicit_missing_path_is_an_error_rather_than_a_silent_fallback() {
        // The user named a file. Falling back to defaults would hide a typo.
        let dir = TempDir::new("missing");
        let error = load(Some(&dir.file("absent.toml")), false)
            .expect_err("a named file that does not exist must fail");
        assert!(matches!(error, ConfigError::Read { .. }), "{error:?}");
    }

    #[test]
    fn an_explicit_valid_path_loads_and_records_its_source() {
        let dir = TempDir::new("explicit");
        let path = dir.write(
            "monitrs.toml",
            "config_version = 1\n[display]\ntheme = \"high-contrast\"\n",
        );
        let loaded = load(Some(&path), false).expect("valid file");
        assert_eq!(loaded.config.display.theme, "high-contrast");
        assert_eq!(loaded.source, ConfigSource::Explicit(path));
    }

    #[test]
    fn a_future_schema_version_is_refused_with_actionable_advice() {
        let dir = TempDir::new("version");
        let path = dir.write("monitrs.toml", "config_version = 99\n");
        let error = load(Some(&path), false).expect_err("unknown version");
        match error {
            ConfigError::UnsupportedVersion { found, advice, .. } => {
                assert_eq!(found, 99);
                assert!(advice.contains("newer monitrs"), "{advice}");
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn an_older_schema_version_gets_different_advice() {
        let dir = TempDir::new("oldversion");
        let path = dir.write("monitrs.toml", "config_version = 0\n");
        match load(Some(&path), false).expect_err("unknown version") {
            ConfigError::UnsupportedVersion { advice, .. } => {
                assert!(advice.contains("config init"), "{advice}");
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn environment_interpolation_is_reported_rather_than_silently_ignored() {
        let dir = TempDir::new("interp");
        let path = dir.write(
            "monitrs.toml",
            "config_version = 1\n[display]\ntheme = \"${MY_THEME}\"\n",
        );
        let loaded = load(Some(&path), false).expect("the value is used literally");
        assert_eq!(loaded.config.display.theme, "${MY_THEME}");
        assert!(
            loaded.warnings.iter().any(|w| w.contains("interpolation")),
            "{:?}",
            loaded.warnings
        );
    }

    // --- init ---------------------------------------------------------------

    #[test]
    fn init_writes_a_valid_file() {
        let dir = TempDir::new("init");
        let path = dir.file("monitrs.toml");
        init_file(&path).expect("writes");
        assert!(path.is_file());
        let (config, _) = read_and_validate(&path).expect("what init wrote must validate");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn init_never_overwrites_an_existing_file() {
        // §12 and §21 M6 both require this explicitly.
        let dir = TempDir::new("nooverwrite");
        let path = dir.write("monitrs.toml", "# mine, hands off\n");
        let error = init_file(&path).expect_err("must refuse");
        assert!(
            matches!(error, ConfigError::AlreadyExists { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("will not overwrite"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("still readable"),
            "# mine, hands off\n"
        );
    }

    #[test]
    fn init_creates_missing_parent_directories() {
        let dir = TempDir::new("mkparent");
        let path = dir.file("nested/deeper/monitrs.toml");
        init_file(&path).expect("writes");
        assert!(path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn an_initialised_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new("perms");
        let path = dir.file("monitrs.toml");
        init_file(&path).expect("writes");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    // --- reload -------------------------------------------------------------

    #[test]
    fn reload_validates_the_whole_candidate_before_returning_it() {
        let dir = TempDir::new("reload");
        let current = Config::default();
        let bad = dir.write(
            "bad.toml",
            "config_version = 1\n[sampling]\ninterval = \"1ms\"\n",
        );
        let error = reload(&current, &bad).expect_err("out of range");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error:?}");
        // The caller still holds an untouched configuration: atomicity is the
        // caller's guarantee, and this is what makes it possible.
        assert_eq!(current, Config::default());
    }

    #[test]
    fn reload_reports_settings_that_cannot_take_effect_until_restart() {
        let dir = TempDir::new("nonreloadable");
        let current = Config::default();
        let path = dir.write("new.toml", "config_version = 1\n[display]\nmouse = true\n");
        let outcome = reload(&current, &path).expect("valid");
        assert!(outcome.config.display.mouse);
        assert!(
            outcome.non_reloadable.contains(&"display.mouse"),
            "{:?}",
            outcome.non_reloadable
        );
    }

    #[test]
    fn a_reload_that_changes_only_reloadable_settings_reports_nothing() {
        let dir = TempDir::new("reloadable");
        let current = Config::default();
        let path = dir.write(
            "new.toml",
            "config_version = 1\n[sampling]\ninterval = \"2s\"\n",
        );
        let outcome = reload(&current, &path).expect("valid");
        assert_eq!(outcome.config.sampling.interval, Duration::from_secs(2));
        assert!(
            outcome.non_reloadable.is_empty(),
            "{:?}",
            outcome.non_reloadable
        );
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn reload_surfaces_parse_warnings_so_they_are_not_lost_on_the_second_read() {
        let dir = TempDir::new("reloadwarn");
        let current = Config::default();
        let path = dir.write(
            "new.toml",
            "config_version = 1\n[display]\ntheme = \"${THEME}\"\n",
        );
        let outcome = reload(&current, &path).expect("the value is used literally");
        assert!(
            outcome.warnings.iter().any(|w| w.contains("interpolation")),
            "{:?}",
            outcome.warnings
        );
    }

    // --- CLI merge ----------------------------------------------------------

    fn cli_from(args: &[&str]) -> Cli {
        use clap::Parser as _;
        Cli::try_parse_from(std::iter::once("monitrs").chain(args.iter().copied())).expect("parses")
    }

    #[test]
    fn cli_values_override_file_values() {
        let mut config: Config = toml::from_str(
            r#"
            config_version = 1
            [display]
            theme = "default-light"
            glyphs = "unicode"
            [sampling]
            interval = "5s"
            "#,
        )
        .expect("parses");

        config.apply_cli(&cli_from(&[
            "--theme",
            "high-contrast",
            "--ascii",
            "--interval",
            "2s",
        ]));

        assert_eq!(config.display.theme, "high-contrast");
        assert_eq!(config.display.glyphs, GlyphModeSetting::Ascii);
        assert_eq!(config.sampling.interval, Duration::from_secs(2));
    }

    #[test]
    fn absent_cli_options_leave_the_file_alone() {
        let original: Config = toml::from_str(
            r#"
            config_version = 1
            [display]
            theme = "default-light"
            [sampling]
            interval = "5s"
            "#,
        )
        .expect("parses");

        let mut merged = original.clone();
        merged.apply_cli(&cli_from(&[]));
        assert_eq!(merged, original, "an empty CLI must not overwrite anything");
    }

    #[test]
    fn no_color_from_the_cli_reaches_the_display_config() {
        let mut config = Config::default();
        config.apply_cli(&cli_from(&["--no-color"]));
        assert_eq!(config.display.color, ColorModeSetting::Off);
    }

    #[test]
    fn a_cli_sort_flag_maps_onto_a_valid_config_token() {
        for flag in [
            "cpu", "memory", "read", "write", "pid", "name", "age", "user", "state", "threads",
        ] {
            let mut config = Config::default();
            config.apply_cli(&cli_from(&["--sort", flag]));
            assert_eq!(config.processes.sort, flag);
            assert!(
                config.validate().is_empty(),
                "{flag}: {:?}",
                config.validate()
            );
        }
    }

    #[test]
    fn library_conversions_preserve_meaning() {
        assert_eq!(ColorMode::from(ColorModeSetting::Off), ColorMode::Off);
        assert_eq!(
            ColorMode::from(ColorModeSetting::Ansi256),
            ColorMode::Ansi256
        );
        assert_eq!(GlyphMode::from(GlyphModeSetting::Ascii), GlyphMode::Ascii);
        assert_eq!(ByteUnits::from(UnitsSetting::Si), ByteUnits::Si);
        assert_eq!(
            CpuNormalization::from(NormalizationSetting::Machine),
            CpuNormalization::Machine
        );
    }

    #[test]
    fn the_numeric_colour_modes_use_their_natural_spelling_in_toml() {
        let config: Config = toml::from_str("[display]\ncolor = \"256\"\n").expect("parses");
        assert_eq!(config.display.color, ColorModeSetting::Ansi256);
        let config: Config = toml::from_str("[display]\ncolor = \"16\"\n").expect("parses");
        assert_eq!(config.display.color, ColorModeSetting::Ansi16);
    }
}
