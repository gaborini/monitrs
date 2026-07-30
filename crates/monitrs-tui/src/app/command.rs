//! The command palette's grammar (§6.3).
//!
//! §6.3 requires palette parsing to be **deterministic, locally implemented, and
//! covered by tests**, and forbids executing arbitrary input with a shell. This
//! module is the whole grammar: a fixed command word, a fixed argument shape, and
//! no fuzzy matching. Nothing here runs anything — [`parse`] returns a
//! [`Command`] value and the reducer decides what it means, which is what keeps
//! `export snapshot` a [`crate::action::Effect`] rather than a file write in a
//! parser.
//!
//! Two deliberate rules:
//!
//! * **No prefix matching.** `so` is not `sort`. A palette that guesses would
//!   change meaning the day a command is added, and §6.3 asks for determinism.
//! * **`filter` and `export snapshot` take the rest of the line verbatim.** A
//!   filter may contain spaces, and so may a path. Splitting them into words
//!   would silently drop half of what the user typed.

use core::time::Duration;
use std::path::PathBuf;
use std::str::FromStr as _;

use monitrs_core::units::parse_duration;
use thiserror::Error;

use crate::action::{SortField, ViewId};
use crate::glyphs::GlyphMode;
use crate::theme::{ColorMode, ThemeId};

/// A parsed palette command (§6.3's initial list).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// `view overview|processes|storage|network|inspect`.
    ChangeView(ViewId),
    /// `sort cpu|memory|read|write|pid|name|age|…`.
    Sort(SortField),
    /// `filter <text>`. An empty argument clears the filter.
    Filter(String),
    /// `interval <duration>`.
    Interval(Duration),
    /// `history <duration>`.
    History(Duration),
    /// `theme <name>`.
    Theme(ThemeId),
    /// `glyphs ascii|unicode|auto`.
    Glyphs(GlyphMode),
    /// `color auto|truecolor|256|16|off`.
    Color(ColorMode),
    /// `export snapshot <path>`.
    ExportSnapshot(PathBuf),
    /// `config path`.
    ConfigPath,
    /// `reload config`.
    ReloadConfig,
    /// `follow [pid]`. No argument follows the selected row.
    ///
    /// The PID is enough: a subtree root is picked out of the process table the user is
    /// looking at, and the reducer refuses a PID that is not in the current sample rather
    /// than scoping the table to nothing. That is also why this carries a bare `u32`
    /// rather than a [`ProcessIdentity`](monitrs_core::model::ProcessIdentity) — nobody
    /// types a start key.
    Follow(Option<u32>),
    /// `unfollow`. Lifts the subtree scope.
    Unfollow,
}

/// Why a typed line is not a command.
///
/// Every variant names the offending text so the notice can quote it back: §12
/// requires an invalid value to point at what was wrong rather than report
/// "invalid input", and the same courtesy applies to the palette.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CommandError {
    /// Nothing was typed.
    #[error("no command")]
    Empty,
    /// The command word is not one of §6.3's.
    ///
    /// A near miss carries a suggestion, but the command is still **rejected**
    /// rather than auto-corrected: §6.3 requires parsing to be deterministic, and
    /// silently running something the user did not type would be worse than a
    /// clear refusal.
    #[error("unknown command {input:?}{}", suggestion.map_or_else(
        || "; press ? for the command list".to_owned(),
        |s| format!("; did you mean `{s}`?"),
    ))]
    Unknown {
        /// What was typed.
        input: String,
        /// The closest known command word, when one is close enough to help.
        suggestion: Option<&'static str>,
    },
    /// The command needs an argument that was not given.
    #[error("`{command}` needs {expected}")]
    MissingArgument {
        /// The command word.
        command: &'static str,
        /// What was expected, phrased to complete the sentence above.
        expected: &'static str,
    },
    /// The argument was given but not understood.
    #[error("`{command}` does not accept {value:?}; expected {expected}")]
    BadArgument {
        /// The command word.
        command: &'static str,
        /// The rejected argument.
        value: String,
        /// What would have been accepted.
        expected: &'static str,
    },
}

/// One line of the palette's suggestion list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandHint {
    /// How the command is written, with its argument placeholder.
    pub usage: &'static str,
    /// The text inserted when this suggestion is completed into the input.
    pub completion: &'static str,
    /// One clause explaining what it does.
    pub summary: &'static str,
}

/// Every command, in the order §6.3 lists them.
///
/// This is the palette's discoverability surface: §6.3 exists so that features do
/// not each need a key, so the list has to be complete and readable.
pub const HINTS: &[CommandHint] = &[
    CommandHint {
        usage: "view <overview|processes|storage|network|inspect>",
        completion: "view ",
        summary: "switch to a main view",
    },
    CommandHint {
        usage: "sort <cpu|memory|read|write|pid|name|age>",
        completion: "sort ",
        summary: "order the process table by a column",
    },
    CommandHint {
        usage: "filter <text>",
        completion: "filter ",
        summary: "match name, command, PID or user; empty clears",
    },
    CommandHint {
        usage: "interval <duration>",
        completion: "interval ",
        summary: "set the sample interval, such as 500ms or 2s",
    },
    CommandHint {
        usage: "history <duration>",
        completion: "history ",
        summary: "set the retained history span, such as 5m",
    },
    CommandHint {
        usage: "theme <default-dark|default-light|high-contrast>",
        completion: "theme ",
        summary: "select a built-in theme",
    },
    CommandHint {
        usage: "glyphs <auto|unicode|ascii>",
        completion: "glyphs ",
        summary: "select the glyph set",
    },
    CommandHint {
        usage: "color <auto|truecolor|256|16|off>",
        completion: "color ",
        summary: "select the colour depth",
    },
    CommandHint {
        usage: "export snapshot <path>",
        completion: "export snapshot ",
        summary: "write a redacted JSON snapshot",
    },
    CommandHint {
        usage: "config path",
        completion: "config path",
        summary: "show where configuration is read from",
    },
    CommandHint {
        usage: "reload config",
        completion: "reload config",
        summary: "re-read the configuration file",
    },
    CommandHint {
        usage: "follow [pid]",
        completion: "follow",
        summary: "scope the table to one process and its descendants",
    },
    CommandHint {
        usage: "unfollow",
        completion: "unfollow",
        summary: "show every process again",
    },
];

/// The suggestions to show for a partially typed line.
///
/// An empty input offers everything. Otherwise a hint matches when either text is
/// a prefix of the other, which keeps the current command's usage on screen while
/// its argument is being typed.
#[must_use]
pub fn hints_for(input: &str) -> Vec<&'static CommandHint> {
    let typed = input.trim_start().to_ascii_lowercase();
    if typed.is_empty() {
        return HINTS.iter().collect();
    }
    HINTS
        .iter()
        .filter(|hint| hint.completion.starts_with(&typed) || typed.starts_with(hint.completion))
        .collect()
}

/// Parses one palette line.
///
/// # Errors
///
/// [`CommandError`] describing exactly which part was not understood.
pub fn parse(line: &str) -> Result<Command, CommandError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CommandError::Empty);
    }
    let (word, rest) = split_word(trimmed);
    let lowered = word.to_ascii_lowercase();

    match lowered.as_str() {
        "view" => {
            let argument = require(rest, "view", "a view name")?;
            ViewId::from_palette_token(argument)
                .map(Command::ChangeView)
                .ok_or_else(|| CommandError::BadArgument {
                    command: "view",
                    value: argument.to_owned(),
                    expected: "one of overview, processes, storage, network, inspect",
                })
        }
        "sort" => {
            let argument = require(rest, "sort", "a column name")?;
            SortField::from_token(argument)
                .map(Command::Sort)
                .ok_or_else(|| CommandError::BadArgument {
                    command: "sort",
                    value: argument.to_owned(),
                    expected: "one of cpu, memory, read, write, pid, name, age, user, state, \
                               threads, virtual, rss, command",
                })
        }
        // The rest of the line verbatim: a filter may contain spaces, and an empty
        // argument is meaningful — `filter` with nothing after it clears it.
        "filter" => Ok(Command::Filter(rest.trim().to_owned())),
        "interval" => {
            let argument = require(rest, "interval", "a duration such as 1s")?;
            parse_duration(argument)
                .map(Command::Interval)
                .map_err(|_| CommandError::BadArgument {
                    command: "interval",
                    value: argument.to_owned(),
                    expected: DURATION_GRAMMAR,
                })
        }
        "history" => {
            let argument = require(rest, "history", "a duration such as 5m")?;
            parse_duration(argument)
                .map(Command::History)
                .map_err(|_| CommandError::BadArgument {
                    command: "history",
                    value: argument.to_owned(),
                    expected: DURATION_GRAMMAR,
                })
        }
        "theme" => {
            let argument = require(rest, "theme", "a theme name")?;
            ThemeId::from_name(argument)
                .map(Command::Theme)
                .ok_or_else(|| CommandError::BadArgument {
                    command: "theme",
                    value: argument.to_owned(),
                    expected: "one of default-dark, default-light, high-contrast",
                })
        }
        "glyphs" => {
            let argument = require(rest, "glyphs", "auto, unicode or ascii")?;
            GlyphMode::from_str(argument)
                .map(Command::Glyphs)
                .map_err(|_| CommandError::BadArgument {
                    command: "glyphs",
                    value: argument.to_owned(),
                    expected: "one of auto, unicode, ascii",
                })
        }
        "color" | "colour" => {
            let argument = require(rest, "color", "auto, truecolor, 256, 16 or off")?;
            ColorMode::from_str(argument)
                .map(Command::Color)
                .map_err(|_| CommandError::BadArgument {
                    command: "color",
                    value: argument.to_owned(),
                    expected: "one of auto, truecolor, 256, 16, off",
                })
        }
        "export" => {
            let (subject, path) = split_word(rest.trim());
            if !subject.eq_ignore_ascii_case("snapshot") {
                return Err(CommandError::BadArgument {
                    command: "export",
                    value: subject.to_owned(),
                    expected: "snapshot",
                });
            }
            let path = path.trim();
            if path.is_empty() {
                return Err(CommandError::MissingArgument {
                    command: "export snapshot",
                    expected: "a file path",
                });
            }
            Ok(Command::ExportSnapshot(PathBuf::from(path)))
        }
        "config" => {
            let argument = require(rest, "config", "the subcommand `path`")?;
            if argument.eq_ignore_ascii_case("path") {
                Ok(Command::ConfigPath)
            } else {
                Err(CommandError::BadArgument {
                    command: "config",
                    value: argument.to_owned(),
                    expected: "path",
                })
            }
        }
        "follow" => {
            let argument = rest.trim();
            if argument.is_empty() {
                // No argument follows what is selected, which is what the `F` key does.
                // Requiring a PID here would make the palette the harder way to do the
                // same thing (§6.3 exists to make features reachable, not ceremonial).
                return Ok(Command::Follow(None));
            }
            argument
                .parse::<u32>()
                .map(|pid| Command::Follow(Some(pid)))
                .map_err(|_| CommandError::BadArgument {
                    command: "follow",
                    value: argument.to_owned(),
                    expected: "a PID, or nothing to follow the selected row",
                })
        }
        "unfollow" => Ok(Command::Unfollow),
        "reload" => {
            let argument = require(rest, "reload", "the subcommand `config`")?;
            if argument.eq_ignore_ascii_case("config") {
                Ok(Command::ReloadConfig)
            } else {
                Err(CommandError::BadArgument {
                    command: "reload",
                    value: argument.to_owned(),
                    expected: "config",
                })
            }
        }
        _ => Err(CommandError::Unknown {
            input: word.to_owned(),
            suggestion: closest_command(word),
        }),
    }
}

/// Splits the first whitespace-delimited word from the rest of the line.
fn split_word(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest),
        None => (line, ""),
    }
}

/// Requires a single-word argument.
fn require<'a>(
    rest: &'a str,
    command: &'static str,
    expected: &'static str,
) -> Result<&'a str, CommandError> {
    let argument = rest.trim();
    if argument.is_empty() {
        return Err(CommandError::MissingArgument { command, expected });
    }
    Ok(argument)
}

/// The duration grammar, quoted back when an argument does not match it.
///
/// `monitrs_core::units::parse_duration` rejects a bare number on purpose — `1` is
/// ambiguous between a second and a millisecond — so the expectation names the
/// units rather than just saying "a duration".
const DURATION_GRAMMAR: &str = "a whole number followed by ms, s, m or h";

/// The known command word within edit distance 2 of `word`.
///
/// Ported from the standalone palette module that was removed as a duplicate of
/// this parser: this was the one thing it had that this did not, and a command
/// palette that refuses `vie` without mentioning `view` is needlessly unhelpful.
#[must_use]
pub(super) fn closest_command(word: &str) -> Option<&'static str> {
    const WORDS: [&str; 11] = [
        "view", "sort", "filter", "interval", "history", "theme", "glyphs", "color", "export",
        "config", "reload",
    ];
    let lower = word.to_ascii_lowercase();
    WORDS
        .into_iter()
        .map(|candidate| (edit_distance(&lower, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, iterative so a long input cannot overflow the stack.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_in_the_specification_parses() {
        // §6.3's initial command list, verbatim.
        assert_eq!(
            parse("view overview"),
            Ok(Command::ChangeView(ViewId::Overview))
        );
        assert_eq!(
            parse("view processes"),
            Ok(Command::ChangeView(ViewId::Processes))
        );
        assert_eq!(
            parse("view storage"),
            Ok(Command::ChangeView(ViewId::Storage))
        );
        assert_eq!(
            parse("view network"),
            Ok(Command::ChangeView(ViewId::Network))
        );
        assert_eq!(
            parse("view inspect"),
            Ok(Command::ChangeView(ViewId::Inspect))
        );
        assert_eq!(parse("sort cpu"), Ok(Command::Sort(SortField::Cpu)));
        assert_eq!(
            parse("sort memory"),
            Ok(Command::Sort(SortField::MemoryShare))
        );
        assert_eq!(parse("sort read"), Ok(Command::Sort(SortField::ReadRate)));
        assert_eq!(parse("sort write"), Ok(Command::Sort(SortField::WriteRate)));
        assert_eq!(parse("sort pid"), Ok(Command::Sort(SortField::Pid)));
        assert_eq!(parse("sort name"), Ok(Command::Sort(SortField::Name)));
        assert_eq!(parse("sort age"), Ok(Command::Sort(SortField::Age)));
        assert_eq!(
            parse("filter rustc"),
            Ok(Command::Filter("rustc".to_owned()))
        );
        assert_eq!(
            parse("interval 500ms"),
            Ok(Command::Interval(Duration::from_millis(500)))
        );
        assert_eq!(
            parse("history 5m"),
            Ok(Command::History(Duration::from_secs(300)))
        );
        assert_eq!(
            parse("theme high-contrast"),
            Ok(Command::Theme(ThemeId::HighContrast))
        );
        assert_eq!(parse("glyphs ascii"), Ok(Command::Glyphs(GlyphMode::Ascii)));
        assert_eq!(
            parse("glyphs unicode"),
            Ok(Command::Glyphs(GlyphMode::Unicode))
        );
        assert_eq!(parse("glyphs auto"), Ok(Command::Glyphs(GlyphMode::Auto)));
        assert_eq!(parse("color auto"), Ok(Command::Color(ColorMode::Auto)));
        assert_eq!(
            parse("color truecolor"),
            Ok(Command::Color(ColorMode::TrueColor))
        );
        assert_eq!(parse("color 256"), Ok(Command::Color(ColorMode::Ansi256)));
        assert_eq!(parse("color 16"), Ok(Command::Color(ColorMode::Ansi16)));
        assert_eq!(parse("color off"), Ok(Command::Color(ColorMode::Off)));
        assert_eq!(
            parse("export snapshot /tmp/monitrs.json"),
            Ok(Command::ExportSnapshot(PathBuf::from("/tmp/monitrs.json")))
        );
        assert_eq!(parse("config path"), Ok(Command::ConfigPath));
        assert_eq!(parse("reload config"), Ok(Command::ReloadConfig));
    }

    #[test]
    fn command_words_are_case_insensitive_and_tolerate_extra_spaces() {
        assert_eq!(
            parse("  VIEW   Storage  "),
            Ok(Command::ChangeView(ViewId::Storage))
        );
        assert_eq!(parse("Reload Config"), Ok(Command::ReloadConfig));
        assert_eq!(parse("colour off"), Ok(Command::Color(ColorMode::Off)));
    }

    #[test]
    fn a_prefix_is_never_guessed_at() {
        for input in ["vie", "so", "s", "exp", "conf", "rel"] {
            assert!(
                matches!(parse(input), Err(CommandError::Unknown { .. })),
                "{input:?} must not be guessed (§6.3 requires determinism)"
            );
        }
    }

    #[test]
    fn a_missing_argument_names_what_was_expected() {
        let error = parse("sort").expect_err("sort needs a column");
        assert_eq!(
            error,
            CommandError::MissingArgument {
                command: "sort",
                expected: "a column name"
            }
        );
        assert!(error.to_string().contains("a column name"));
        assert!(matches!(
            parse("export snapshot"),
            Err(CommandError::MissingArgument {
                command: "export snapshot",
                ..
            })
        ));
    }

    #[test]
    fn a_bad_argument_quotes_the_offending_value() {
        let error = parse("sort fortnight").expect_err("not a column");
        let message = error.to_string();
        assert!(message.contains("fortnight"), "{message}");
        assert!(
            message.contains("cpu"),
            "the message lists what is accepted"
        );

        let error = parse("interval 1").expect_err("a bare number is ambiguous");
        assert!(error.to_string().contains("ms, s, m or h"));
    }

    #[test]
    fn filter_takes_the_rest_of_the_line_including_spaces() {
        assert_eq!(
            parse("filter cargo build --release"),
            Ok(Command::Filter("cargo build --release".to_owned()))
        );
        assert_eq!(parse("filter"), Ok(Command::Filter(String::new())));
        assert_eq!(parse("filter   "), Ok(Command::Filter(String::new())));
    }

    #[test]
    fn an_export_path_may_contain_spaces() {
        assert_eq!(
            parse("export snapshot /tmp/my snapshot.json"),
            Ok(Command::ExportSnapshot(PathBuf::from(
                "/tmp/my snapshot.json"
            )))
        );
    }

    #[test]
    fn export_rejects_anything_but_snapshot() {
        assert!(matches!(
            parse("export everything /tmp/x"),
            Err(CommandError::BadArgument {
                command: "export",
                ..
            })
        ));
    }

    #[test]
    fn an_empty_line_is_not_a_command() {
        assert_eq!(parse(""), Err(CommandError::Empty));
        assert_eq!(parse("   "), Err(CommandError::Empty));
    }

    #[test]
    fn nothing_in_the_grammar_can_execute_a_shell() {
        // §6.3: do not execute arbitrary input with a shell. There is no command
        // word that takes a program, so the worst a hostile line can do is fail.
        for input in [
            "!rm -rf /",
            "$(whoami)",
            "; shutdown now",
            "view overview; rm -rf /",
        ] {
            let parsed = parse(input);
            assert!(
                matches!(
                    parsed,
                    Err(CommandError::Unknown { .. }) | Err(CommandError::BadArgument { .. })
                ),
                "{input:?} parsed as {parsed:?}"
            );
        }
    }

    #[test]
    fn the_hint_list_covers_every_command_and_completes_to_something_parseable() {
        // §6.3's eleven, plus `follow` and `unfollow`, which have no section of their
        // own there: the specification asks for a way to watch a process tree without
        // saying how it is reached, and the palette is where a feature goes when it does
        // not warrant its own key. The count is asserted so a new command cannot be
        // added without a hint, which is the discoverability surface §6.3 exists for.
        assert_eq!(HINTS.len(), 13, "§6.3's eleven, plus follow and unfollow");
        for hint in HINTS {
            assert!(hint.usage.is_ascii(), "§5.1: hints must be ASCII-safe");
            assert!(!hint.summary.is_empty());
            assert!(
                hint.usage.starts_with(hint.completion.trim_end()),
                "{} does not complete towards {}",
                hint.completion,
                hint.usage
            );
        }
    }

    #[test]
    fn suggestions_narrow_as_the_command_word_is_typed() {
        assert_eq!(hints_for("").len(), HINTS.len());
        assert_eq!(hints_for("  ").len(), HINTS.len());

        let sorting = hints_for("sor");
        assert_eq!(sorting.len(), 1);
        assert_eq!(sorting.first().map(|hint| hint.completion), Some("sort "));

        let typing_argument = hints_for("sort cp");
        assert_eq!(
            typing_argument.len(),
            1,
            "the usage stays visible while the argument is typed"
        );

        assert!(hints_for("zzz").is_empty());
    }

    #[test]
    fn export_and_config_share_prefixes_without_colliding() {
        let exports = hints_for("export");
        assert_eq!(exports.len(), 1);
        assert_eq!(
            exports.first().map(|hint| hint.completion),
            Some("export snapshot ")
        );
        assert_eq!(hints_for("config").len(), 1);
        assert_eq!(
            hints_for("c").len(),
            2,
            "color and config both start with c"
        );
    }

    #[test]
    fn a_near_miss_is_still_refused_but_names_the_command_it_resembles() {
        // §6.3 requires determinism: the suggestion is advice, never a correction.
        let error = parse("vie overview").expect_err("`vie` is not a command");
        match &error {
            CommandError::Unknown { input, suggestion } => {
                assert_eq!(input, "vie");
                assert_eq!(*suggestion, Some("view"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        let message = error.to_string();
        assert!(message.contains("did you mean `view`?"), "{message}");
    }

    #[test]
    fn a_word_resembling_nothing_gets_no_suggestion_and_points_at_the_list() {
        let error = parse("xyzzy").expect_err("not a command");
        match &error {
            CommandError::Unknown { suggestion, .. } => assert_eq!(*suggestion, None),
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert!(error.to_string().contains("press ? for the command list"));
    }

    #[test]
    fn suggestions_are_case_insensitive_and_bounded() {
        assert_eq!(closest_command("VIEW"), Some("view"));
        assert_eq!(closest_command("sortt"), Some("sort"));
        assert_eq!(closest_command("colour"), Some("color"));
        // Two edits is the limit; three is not a near miss.
        assert_eq!(closest_command("xxxxxx"), None);
        assert_eq!(closest_command(""), None);
    }
}
