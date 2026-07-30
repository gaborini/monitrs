//! Command-line interface.
//!
//! Every option is an `Option<T>`, so "not given" is distinguishable from "given
//! the default value". That distinction is what lets §12's rule — *CLI values
//! override file values* — be implemented as a merge rather than as a guess.
//!
//! The value enums here are deliberately local to the CLI rather than reused from
//! `monitrs-tui` or `monitrs-core`. A CLI is a stable user-facing surface: the
//! flag spelling must not change because an internal enum was refactored, and
//! `clap::ValueEnum` cannot be derived for a type from another crate anyway. The
//! conversions live in one place, the configuration merge, so there is exactly
//! one translation point.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use monitrs_core::units::{DurationParseError, parse_duration};

/// The bounds §8.5 places on the sample interval.
pub(crate) const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// The bounds §8.5 places on the sample interval.
pub(crate) const MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// The bounds §8.5 places on history retention.
pub(crate) const MIN_HISTORY: std::time::Duration = std::time::Duration::from_secs(30);
/// The bounds §8.5 places on history retention in v1.
pub(crate) const MAX_HISTORY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// A fast, keyboard-first system cockpit for Linux and macOS.
#[derive(Debug, Parser)]
#[command(
    name = "monitrs",
    version,
    about,
    long_about = "monitrs shows what your machine is doing now and what it was doing a few \
                  minutes ago. Pause the timeline with Space, scrub with [ and ], and return \
                  to live with L.\n\nEvery metric carries its own availability: a value monitrs \
                  cannot measure is shown as `warming up`, `permission denied`, or `n/a`, never \
                  as 0.",
    // Long help is worth reading; short help should fit a terminal.
    max_term_width = 100
)]
pub(crate) struct Cli {
    /// What to do instead of launching the interface.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    #[command(flatten)]
    pub(crate) display: DisplayArgs,

    #[command(flatten)]
    pub(crate) sampling: SamplingArgs,

    #[command(flatten)]
    pub(crate) view: ViewArgs,

    #[command(flatten)]
    pub(crate) config: ConfigArgs,
}

/// Subcommands that do something other than launch the interface.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Inspect and manage the configuration file.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Generate a shell completion script on stdout.
    Completions {
        /// Which shell to generate for.
        shell: clap_complete::Shell,
    },

    /// Generate a roff man page on stdout.
    Manpage,

    /// Take one snapshot, print it, and exit.
    ///
    /// Useful for scripting and for bug reports. Environment variable values are
    /// never included, and command arguments are redacted unless
    /// `--include-arguments` is passed (§15.2).
    Snapshot {
        /// Output format.
        #[arg(long, value_enum, default_value_t = SnapshotFormat::Json)]
        format: SnapshotFormat,

        /// Write to a file instead of stdout.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,

        /// Include full command lines.
        ///
        /// Off by default: process arguments frequently contain credentials, and
        /// an export is something people paste into issues (§15.2).
        #[arg(long)]
        include_arguments: bool,

        /// How many samples to take before printing.
        ///
        /// The default of 2 exists because delta-based metrics are `warming up`
        /// in the first sample (§8.2); one sample would export an almost empty
        /// snapshot and look like a bug.
        #[arg(long, value_name = "N", default_value_t = 2)]
        samples: u8,
    },
}

/// `monitrs config ...`
#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Print the path monitrs reads configuration from.
    Path,

    /// Write a documented starter configuration file.
    ///
    /// Never overwrites an existing file (§12, §21 M6).
    Init {
        /// Write somewhere other than the default location.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },

    /// Validate a configuration file without launching.
    Check {
        /// Validate this file instead of the default location.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

/// Appearance overrides.
#[derive(Args, Debug)]
#[command(next_help_heading = "Display")]
pub(crate) struct DisplayArgs {
    /// Glyph set to render with.
    ///
    /// `auto` uses Unicode when the locale is UTF-8 and falls back to strict
    /// 7-bit ASCII otherwise.
    #[arg(long, value_enum, value_name = "MODE")]
    pub(crate) glyphs: Option<GlyphModeArg>,

    /// Shorthand for `--glyphs ascii`.
    #[arg(long, conflicts_with = "glyphs")]
    pub(crate) ascii: bool,

    /// Colour depth to use.
    ///
    /// `auto` respects the NO_COLOR convention; passing this flag explicitly
    /// overrides NO_COLOR (§5.2).
    #[arg(long, value_enum, value_name = "MODE")]
    pub(crate) color: Option<ColorModeArg>,

    /// Shorthand for `--color off`.
    #[arg(long, conflicts_with = "color")]
    pub(crate) no_color: bool,

    /// Theme name, e.g. `default-dark`, `default-light`, `high-contrast`.
    #[arg(long, value_name = "NAME")]
    pub(crate) theme: Option<String>,

    /// Byte unit family.
    #[arg(long, value_enum, value_name = "FAMILY")]
    pub(crate) units: Option<UnitsArg>,

    /// Enable mouse support.
    #[arg(long)]
    pub(crate) mouse: bool,
}

/// Sampling overrides.
#[derive(Args, Debug)]
#[command(next_help_heading = "Sampling")]
pub(crate) struct SamplingArgs {
    /// How often to sample, e.g. `1s`, `500ms`.
    #[arg(long, value_name = "DURATION", value_parser = duration_arg)]
    pub(crate) interval: Option<std::time::Duration>,

    /// How much history to retain, e.g. `5m`, `30s`.
    #[arg(long, value_name = "DURATION", value_parser = duration_arg)]
    pub(crate) history: Option<std::time::Duration>,
}

/// What to show on launch.
#[derive(Args, Debug)]
#[command(next_help_heading = "View")]
pub(crate) struct ViewArgs {
    /// Start on the Processes screen.
    #[arg(long)]
    pub(crate) processes: bool,

    /// Start with the process tree expanded.
    #[arg(long)]
    pub(crate) tree: bool,

    /// Start with this process filter applied.
    #[arg(long, value_name = "TEXT")]
    pub(crate) filter: Option<String>,

    /// Start sorted by this column.
    #[arg(long, value_enum, value_name = "FIELD")]
    pub(crate) sort: Option<SortFieldArg>,

    /// Show per-core CPU on the Overview screen.
    #[arg(long)]
    pub(crate) per_core: bool,
}

/// Configuration and diagnostics.
#[derive(Args, Debug)]
#[command(next_help_heading = "Configuration")]
pub(crate) struct ConfigArgs {
    /// Read configuration from this file instead of the default location.
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    pub(crate) config: Option<PathBuf>,

    /// Ignore any configuration file and use built-in defaults.
    #[arg(long)]
    pub(crate) no_config: bool,

    /// Write a debug log to this file.
    ///
    /// Off by default. Nothing is ever written to stdout or stderr while the
    /// interface is running, because that would corrupt the display (§14.2).
    /// Command lines are redacted and environment values are never logged.
    #[arg(long, value_name = "PATH")]
    pub(crate) debug_log: Option<PathBuf>,
}

/// `--glyphs` values (§5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum GlyphModeArg {
    /// Unicode when the locale supports it, strict ASCII otherwise.
    Auto,
    /// Box drawing, blocks, and Braille.
    Unicode,
    /// Strict printable 7-bit ASCII only.
    Ascii,
}

/// `--color` values (§5.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum ColorModeArg {
    /// Detect from the terminal, honouring NO_COLOR.
    Auto,
    /// 24-bit colour.
    Truecolor,
    /// 256-colour palette.
    #[value(name = "256")]
    Ansi256,
    /// 16-colour palette.
    #[value(name = "16")]
    Ansi16,
    /// No colour at all. Meaning is carried by symbols and text.
    Off,
}

/// `--units` values (§5.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum UnitsArg {
    /// Powers of 1024: KiB, MiB, GiB. The default.
    Iec,
    /// Powers of 1000: kB, MB, GB.
    Si,
}

/// `--sort` values (§7.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum SortFieldArg {
    /// CPU percentage.
    Cpu,
    /// Resident memory.
    Memory,
    /// Disk read rate.
    Read,
    /// Disk write rate.
    Write,
    /// Process id.
    Pid,
    /// Process name.
    Name,
    /// Process age.
    Age,
    /// Owning user.
    User,
    /// Scheduling state.
    State,
    /// Thread count.
    Threads,
}

/// `monitrs snapshot --format` values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum SnapshotFormat {
    /// Machine-readable JSON.
    Json,
}

/// Parses and range-checks a duration argument.
///
/// Bounds are enforced here rather than after merging so that an out-of-range
/// value is reported by clap against the flag the user actually typed, which is
/// §12's requirement that an invalid value point at the exact key.
fn duration_arg(raw: &str) -> Result<std::time::Duration, String> {
    let parsed = parse_duration(raw).map_err(format_duration_error)?;
    Ok(parsed)
}

fn format_duration_error(error: DurationParseError) -> String {
    error.to_string()
}

impl Cli {
    /// Resolves `--ascii` into the equivalent `--glyphs` value.
    ///
    /// The shorthand exists because `--ascii` is what people reach for over SSH;
    /// collapsing it here means the rest of the program has one representation.
    #[must_use]
    pub(crate) fn glyph_mode(&self) -> Option<GlyphModeArg> {
        if self.display.ascii {
            return Some(GlyphModeArg::Ascii);
        }
        self.display.glyphs
    }

    /// Resolves `--no-color` into the equivalent `--color` value.
    #[must_use]
    pub(crate) fn color_mode(&self) -> Option<ColorModeArg> {
        if self.display.no_color {
            return Some(ColorModeArg::Off);
        }
        self.display.color
    }

    /// Whether the user explicitly asked for a colour mode.
    ///
    /// §5.2 requires NO_COLOR to be honoured *unless* an explicit flag overrides
    /// it, so the resolver needs to know the difference between "auto" chosen by
    /// default and "auto" typed on the command line.
    #[must_use]
    pub(crate) const fn color_was_explicit(&self) -> bool {
        self.display.no_color || self.display.color.is_some()
    }

    /// Validates the interval and history bounds §8.5 defines.
    ///
    /// Returns every problem found rather than only the first, so a user fixing
    /// two bad flags does not have to run the program twice.
    #[must_use]
    pub(crate) fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(interval) = self.sampling.interval
            && !(MIN_INTERVAL..=MAX_INTERVAL).contains(&interval)
        {
            problems.push(format!(
                "--interval must be between {} and {}, got {}",
                monitrs_core::units::format_duration(MIN_INTERVAL),
                monitrs_core::units::format_duration(MAX_INTERVAL),
                monitrs_core::units::format_duration(interval),
            ));
        }
        if let Some(history) = self.sampling.history
            && !(MIN_HISTORY..=MAX_HISTORY).contains(&history)
        {
            problems.push(format!(
                "--history must be between {} and {}, got {}",
                monitrs_core::units::format_duration(MIN_HISTORY),
                monitrs_core::units::format_duration(MAX_HISTORY),
                monitrs_core::units::format_duration(history),
            ));
        }
        if let (Some(interval), Some(history)) = (self.sampling.interval, self.sampling.history)
            && history < interval
        {
            problems.push(format!(
                "--history ({}) must be at least one --interval ({})",
                monitrs_core::units::format_duration(history),
                monitrs_core::units::format_duration(interval),
            ));
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;
    use std::time::Duration;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("monitrs").chain(args.iter().copied()))
            .expect("arguments should parse")
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(std::iter::once("monitrs").chain(args.iter().copied()))
            .expect_err("arguments should be rejected")
    }

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // Catches duplicate flags, bad value-enum names, and conflicting IDs at
        // test time rather than at first run.
        Cli::command().debug_assert();
    }

    #[test]
    fn launching_with_no_arguments_is_valid_and_selects_no_subcommand() {
        let cli = parse(&[]);
        assert!(cli.command.is_none());
        assert!(cli.validate().is_empty());
        assert!(
            cli.glyph_mode().is_none(),
            "absence must be distinguishable"
        );
        assert!(cli.color_mode().is_none());
    }

    #[test]
    fn help_and_version_are_available() {
        // Both are clap-generated "errors" that print and exit successfully.
        assert_eq!(
            parse_err(&["--help"]).kind(),
            clap::error::ErrorKind::DisplayHelp
        );
        assert_eq!(
            parse_err(&["--version"]).kind(),
            clap::error::ErrorKind::DisplayVersion
        );
    }

    #[test]
    fn ascii_and_no_color_are_shorthands_for_their_long_forms() {
        assert_eq!(parse(&["--ascii"]).glyph_mode(), Some(GlyphModeArg::Ascii));
        assert_eq!(parse(&["--no-color"]).color_mode(), Some(ColorModeArg::Off));
        assert_eq!(
            parse(&["--glyphs", "unicode"]).glyph_mode(),
            Some(GlyphModeArg::Unicode)
        );
    }

    #[test]
    fn a_shorthand_conflicting_with_its_long_form_is_rejected() {
        // Silently letting one win would make the behaviour unpredictable.
        assert_eq!(
            parse_err(&["--ascii", "--glyphs", "unicode"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert_eq!(
            parse_err(&["--no-color", "--color", "truecolor"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn config_and_no_config_cannot_be_combined() {
        assert_eq!(
            parse_err(&["--config", "/tmp/x.toml", "--no-config"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn an_explicit_color_flag_is_distinguishable_from_the_default() {
        assert!(!parse(&[]).color_was_explicit());
        assert!(parse(&["--color", "auto"]).color_was_explicit());
        assert!(parse(&["--no-color"]).color_was_explicit());
    }

    #[test]
    fn numeric_color_modes_use_their_natural_spelling() {
        assert_eq!(
            parse(&["--color", "256"]).color_mode(),
            Some(ColorModeArg::Ansi256)
        );
        assert_eq!(
            parse(&["--color", "16"]).color_mode(),
            Some(ColorModeArg::Ansi16)
        );
    }

    #[test]
    fn durations_accept_the_documented_grammar() {
        assert_eq!(
            parse(&["--interval", "500ms"]).sampling.interval,
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse(&["--history", "5m"]).sampling.history,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn an_unparsable_duration_is_rejected_with_the_offending_text() {
        let error = parse_err(&["--interval", "1"]);
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        let message = error.to_string();
        assert!(message.contains("interval"), "{message}");
    }

    #[test]
    fn out_of_range_intervals_are_reported_against_the_flag_that_caused_them() {
        let problems = parse(&["--interval", "100ms"]).validate();
        assert_eq!(problems.len(), 1);
        let message = problems.first().expect("one problem");
        assert!(message.contains("--interval"), "{message}");
        assert!(message.contains("250ms"), "{message}");

        assert!(
            parse(&["--interval", "250ms"]).validate().is_empty(),
            "the bound is inclusive"
        );
        assert!(
            parse(&["--interval", "60s"]).validate().is_empty(),
            "the bound is inclusive"
        );
        assert_eq!(parse(&["--interval", "61s"]).validate().len(), 1);
    }

    #[test]
    fn out_of_range_history_is_reported() {
        assert_eq!(parse(&["--history", "10s"]).validate().len(), 1);
        assert_eq!(parse(&["--history", "2h"]).validate().len(), 1);
        assert!(parse(&["--history", "30s"]).validate().is_empty());
        assert!(parse(&["--history", "1h"]).validate().is_empty());
    }

    #[test]
    fn history_shorter_than_one_interval_is_rejected() {
        let problems = parse(&["--interval", "60s", "--history", "30s"]).validate();
        assert_eq!(problems.len(), 1);
        assert!(
            problems
                .first()
                .is_some_and(|p| p.contains("at least one --interval")),
            "{problems:?}"
        );
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let problems = parse(&["--interval", "100ms", "--history", "10s"]).validate();
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn snapshot_export_redacts_arguments_by_default() {
        let cli = parse(&["snapshot"]);
        let Some(Command::Snapshot {
            include_arguments,
            samples,
            format,
            ..
        }) = cli.command
        else {
            panic!("expected the snapshot subcommand");
        };
        assert!(!include_arguments, "§15.2: arguments may contain secrets");
        assert_eq!(samples, 2, "one sample would export only warming-up values");
        assert_eq!(format, SnapshotFormat::Json);
    }

    #[test]
    fn snapshot_can_be_asked_to_include_arguments_explicitly() {
        let cli = parse(&["snapshot", "--include-arguments"]);
        let Some(Command::Snapshot {
            include_arguments, ..
        }) = cli.command
        else {
            panic!("expected the snapshot subcommand");
        };
        assert!(include_arguments);
    }

    #[test]
    fn config_subcommands_parse() {
        assert!(matches!(
            parse(&["config", "path"]).command,
            Some(Command::Config(ConfigCommand::Path))
        ));
        assert!(matches!(
            parse(&["config", "init"]).command,
            Some(Command::Config(ConfigCommand::Init { path: None }))
        ));
        let cli = parse(&["config", "check", "/tmp/monitrs.toml"]);
        let Some(Command::Config(ConfigCommand::Check { path: Some(path) })) = cli.command else {
            panic!("expected config check with a path");
        };
        assert_eq!(path, PathBuf::from("/tmp/monitrs.toml"));
    }

    #[test]
    fn completions_and_manpage_parse() {
        assert!(matches!(
            parse(&["completions", "zsh"]).command,
            Some(Command::Completions {
                shell: clap_complete::Shell::Zsh
            })
        ));
        assert!(matches!(
            parse(&["manpage"]).command,
            Some(Command::Manpage)
        ));
    }

    #[test]
    fn every_sort_field_in_the_specification_is_accepted() {
        for (flag, expected) in [
            ("cpu", SortFieldArg::Cpu),
            ("memory", SortFieldArg::Memory),
            ("read", SortFieldArg::Read),
            ("write", SortFieldArg::Write),
            ("pid", SortFieldArg::Pid),
            ("name", SortFieldArg::Name),
            ("age", SortFieldArg::Age),
            ("user", SortFieldArg::User),
            ("state", SortFieldArg::State),
            ("threads", SortFieldArg::Threads),
        ] {
            assert_eq!(
                parse(&["--sort", flag]).view.sort,
                Some(expected),
                "--sort {flag}"
            );
        }
    }

    #[test]
    fn an_unknown_sort_field_is_rejected_rather_than_ignored() {
        assert_eq!(
            parse_err(&["--sort", "entropy"]).kind(),
            clap::error::ErrorKind::InvalidValue
        );
    }
}
