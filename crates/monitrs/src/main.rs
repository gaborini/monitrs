//! monitrs — a fast, keyboard-first system cockpit for Linux and macOS.
//!
//! This binary is the only place that knows about all three libraries at once.
//! It owns the clock, the threads, the channels, and the execution of effects;
//! the libraries own the data, the rendering, and the decisions (§10.1).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use clap::{CommandFactory as _, Parser as _};
use monitrs_collectors::{CommonCollector, DueTiers, SampleTick, SnapshotSource as _};

mod cli;
mod export;

use cli::{Cli, Command, SnapshotFormat};
use export::{RedactionPolicy, SnapshotExport};

/// Exit code for a usage error, matching the convention `clap` itself uses.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    // Installed first so that a panic during startup still produces a readable
    // report. Once the terminal guard is active, its hook chains onto this one to
    // restore the terminal *before* the report is printed (§14.3).
    if let Err(error) = color_eyre::install() {
        // Losing pretty reports is not worth refusing to start over.
        eprintln!("monitrs: could not install the error reporter: {error}");
    }

    let cli = Cli::parse();

    // Range checks live outside clap's parser so that every problem is reported
    // at once rather than one per run (§12).
    let problems = cli.validate();
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("monitrs: {problem}");
        }
        return ExitCode::from(EXIT_USAGE);
    }

    match run(&cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("monitrs: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> color_eyre::Result<ExitCode> {
    match &cli.command {
        Some(Command::Completions { shell }) => {
            // Generated from the same `Cli` definition the program parses, so
            // completions cannot drift from the real flags (§21 M6).
            let mut command = Cli::command();
            let name = command.get_name().to_owned();
            clap_complete::generate(*shell, &mut command, name, &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }

        Some(Command::Manpage) => {
            let man = clap_mangen::Man::new(Cli::command());
            let mut buffer = Vec::new();
            man.render(&mut buffer)?;
            std::io::stdout().write_all(&buffer)?;
            Ok(ExitCode::SUCCESS)
        }

        Some(Command::Config(_)) => Err(color_eyre::eyre::eyre!(
            "configuration handling is not available in this build. \
             CHANGELOG.md lists what currently works."
        )),

        Some(Command::Snapshot {
            format,
            output,
            include_arguments,
            samples,
        }) => run_snapshot(
            cli,
            *format,
            output.as_deref(),
            *include_arguments,
            *samples,
        ),

        None => Err(color_eyre::eyre::eyre!(
            "the interactive interface is not available in this build. \
             `monitrs --help`, `monitrs completions <SHELL>`, and `monitrs manpage` work today; \
             CHANGELOG.md lists the rest."
        )),
    }
}

/// The shortest gap between two samples that still yields a real CPU delta.
///
/// `sysinfo` needs a minimum interval between CPU reads; below it, the second
/// sample would report `warming up` forever and the export would look broken.
const MIN_SAMPLE_GAP: Duration = Duration::from_millis(250);

fn run_snapshot(
    cli: &Cli,
    format: SnapshotFormat,
    output: Option<&std::path::Path>,
    include_arguments: bool,
    samples: u8,
) -> color_eyre::Result<ExitCode> {
    if samples == 0 {
        return Err(color_eyre::eyre::eyre!("--samples must be at least 1"));
    }
    let SnapshotFormat::Json = format;

    let gap = cli
        .sampling
        .interval
        .unwrap_or(MIN_SAMPLE_GAP)
        .max(MIN_SAMPLE_GAP);

    let mut collector = CommonCollector::new()?;
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::now());
    let mut latest = collector.sample(&tick)?;

    for _ in 1..samples {
        std::thread::sleep(gap);
        tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        latest = collector.sample(&tick)?;
    }

    let policy = if include_arguments {
        RedactionPolicy::FULL
    } else {
        RedactionPolicy::REDACTED
    };
    let json = SnapshotExport::new(&latest, policy).to_json()?;

    match output {
        Some(path) => {
            std::fs::write(path, json.as_bytes())?;
            // Config and exports use user-only permissions where the platform
            // supports it (§15.2): an export can contain command lines.
            restrict_to_user(path)?;
            eprintln!("monitrs: wrote {} bytes to {}", json.len(), path.display());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(json.as_bytes())?;
            stdout.write_all(b"\n")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Restricts a written file to the current user where the platform supports it.
#[cfg(unix)]
fn restrict_to_user(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Non-Unix platforms are not v1 targets; leaving permissions alone is honest.
#[cfg(not(unix))]
fn restrict_to_user(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_generate_for_every_supported_shell() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Elvish,
        ] {
            let mut command = Cli::command();
            let mut output = Vec::new();
            clap_complete::generate(shell, &mut command, "monitrs", &mut output);
            let script = String::from_utf8(output).expect("completions are valid UTF-8");
            assert!(!script.is_empty(), "{shell} produced nothing");
            assert!(
                script.contains("monitrs"),
                "{shell} script does not mention the binary"
            );
        }
    }

    #[test]
    fn completions_mention_the_real_flags_so_they_cannot_drift() {
        let mut command = Cli::command();
        let mut output = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut command,
            "monitrs",
            &mut output,
        );
        let script = String::from_utf8(output).expect("valid UTF-8");
        for flag in [
            "--interval",
            "--history",
            "--glyphs",
            "--color",
            "--ascii",
            "--no-color",
        ] {
            assert!(script.contains(flag), "completions omit {flag}");
        }
    }

    #[test]
    fn the_man_page_renders_and_describes_the_binary() {
        let man = clap_mangen::Man::new(Cli::command());
        let mut buffer = Vec::new();
        man.render(&mut buffer).expect("man page renders");
        let page = String::from_utf8(buffer).expect("roff is valid UTF-8");
        assert!(page.contains("monitrs"));
        assert!(
            page.contains("system cockpit"),
            "the description should reach the man page"
        );
    }
}
