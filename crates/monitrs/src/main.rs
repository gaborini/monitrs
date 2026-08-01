//! monitrs — a fast, keyboard-first system cockpit for Linux and macOS.
//!
//! This binary is the only place that knows about all three libraries at once.
//! It owns the clock, the threads, the channels, and the execution of effects;
//! the libraries own the data, the rendering, and the decisions (§10.1).
//!
//! # Why logging is installed here
//!
//! `--debug-log` is a promise, and a promise kept on one code path only is worse
//! than no promise at all: it produces a flag that appears to work and does not.
//! So the log is installed in [`main`], once, before the subcommand is dispatched,
//! and closed after the subcommand returns — which on the interactive path is
//! after the terminal has been restored, because `tracing-appender`'s final flush
//! can print if it times out (§14.2). What differs between paths is only *where*
//! the lines may go: see [`log_sinks`].

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use clap::{CommandFactory as _, Parser as _};
use monitrs_collectors::{DueTiers, SampleTick, SnapshotSource, platform_collector};

mod cli;
mod config;
mod export;
mod interactive;
mod logging;
mod overhead;
mod runtime;
mod signals;

use cli::{Cli, Command, ConfigCommand, SnapshotFormat};
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

    // Installed for every subcommand, not only the interactive one (§14.2). A log
    // that cannot be opened is reported and then ignored: §14.1 keeps that firmly
    // out of the fatal-startup-error class.
    let startup = logging::install(&logging::settings_for(
        cli.config.debug_log.as_deref(),
        log_sinks(cli.command.as_ref()),
    ));
    // Nothing owns the terminal yet, so these reach stderr. Whatever could not be
    // printed is handed to the interactive runtime, which shows it as a notice once
    // there is somewhere to show it.
    let deferred = logging::report_problems(&startup.problems);

    let code = match run(&cli, deferred) {
        Ok(code) => code,
        // Whoever was reading stdout closed it — `monitrs snapshot | head -3`, and
        // every other pipeline whose reader has seen enough. That is the reader's
        // decision, not a failure of ours, so it earns silence and a successful
        // status rather than an error message and `ExitCode::FAILURE`. Reporting it
        // was worse than noisy: under the `set -o pipefail` any careful script
        // sets, it turned an ordinary pipeline into a failed one.
        Err(error) if is_broken_pipe(&error) => {
            // Recorded, because `--debug-log` should still explain why a run ended
            // with less output than the reader asked for — but at debug level, since
            // nothing went wrong.
            tracing::debug!("stdout was closed by the reader; exiting quietly");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let reason = format!("{error:#}");
            // Recorded as well as printed, so a bug report that includes the log
            // carries the failure and not merely the run-up to it (§14.1). Printed
            // unconditionally: `--debug-log` only ever *adds* output, so a stderr
            // mirror showing the same failure a second time is preferable to the
            // flag quietly reshaping monitrs' normal error message.
            tracing::error!(%reason, "monitrs is exiting without completing the request");
            eprintln!("monitrs: {reason}");
            ExitCode::FAILURE
        }
    };

    // Closed last, deliberately. Every path that took the terminal has restored it
    // by the time it returned, so `tracing-appender`'s guard can no longer print
    // onto an alternate screen (§14.2, and the ordering `interactive::run`
    // documents).
    if let Some(log) = startup.log {
        log.shutdown();
    }
    code
}

/// Whether this failure is only "the reader of our stdout went away".
///
/// Deliberately narrow: the error itself must *be* the broken pipe. Every write to
/// stdout in this binary is a bare `write_all(..)?` or `writeln!(..)?`, so that is
/// the shape a closed reader actually produces, and `eyre`'s downcast already looks
/// through the context a `wrap_err` adds — so adding a context line above one of
/// those writes would not break this.
///
/// What it will *not* do is search an error's `source()` chain, and that is the
/// point rather than an omission. A `BrokenPipe` reachable only as some other
/// error's cause did not come from writing our own output; treating it as "the
/// reader left" would exit 0 and print nothing on a run that genuinely failed. The
/// cost of being wrong in that direction is a silently swallowed failure, which is
/// worse than the noisy message this replaces.
///
/// Rust's runtime sets `SIGPIPE` to `SIG_IGN` before `main`, which is why this is a
/// recoverable `io::Error` at all: without that the process would simply die of the
/// signal, the way `yes | head` does. Restoring the default disposition would match
/// that convention more exactly, but it is not available here — it means calling
/// `libc::signal`, and this crate carries `#![forbid(unsafe_code)]`. It would also
/// apply to the interactive path, where dying mid-frame would leave the terminal in
/// whatever state the alternate screen left it. Recognising the error is both the
/// narrower change and the only one the crate's own lints permit.
fn is_broken_pipe(error: &color_eyre::Report) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Which sinks the debug log may use on the path this invocation will take (§14.2).
///
/// §14.2's prohibition is about the alternate screen: nothing may reach stdout or
/// stderr while it is active. Only the interactive path activates it, so a one-shot
/// subcommand can mirror the log to stderr — which is where someone running
/// `monitrs snapshot --debug-log` is looking, and it means a log is useful even when
/// the run is too short to go and read the file. stdout is never used on either
/// path: on the snapshot path it carries the export itself.
fn log_sinks(command: Option<&Command>) -> logging::Sinks {
    match command {
        None => logging::Sinks::FileOnly,
        Some(_) => logging::Sinks::FileAndStderr,
    }
}

/// Dispatches the subcommand.
///
/// `log_notices` carries the logging problems that could not be printed, which only
/// the interactive path can display; every other path has already had them on
/// stderr (§14.2).
fn run(cli: &Cli, log_notices: Vec<String>) -> color_eyre::Result<ExitCode> {
    match &cli.command {
        Some(Command::Completions { shell }) => {
            // Generated from the same `Cli` definition the program parses, so
            // completions cannot drift from the real flags (§21 M6).
            let mut command = Cli::command();
            let name = command.get_name().to_owned();
            // Rendered into a buffer first, like `manpage` below, because
            // `clap_complete::generate` writes straight to the sink and *panics* if
            // that write fails. Handing it a `Vec` makes the only fallible write a
            // plain `?`, so a closed pipe reaches the handler in `main` as an error
            // to be recognised rather than a panic report.
            let mut buffer = Vec::new();
            clap_complete::generate(*shell, &mut command, name, &mut buffer);
            std::io::stdout().write_all(&buffer)?;
            Ok(ExitCode::SUCCESS)
        }

        Some(Command::Manpage) => {
            let man = clap_mangen::Man::new(Cli::command());
            let mut buffer = Vec::new();
            man.render(&mut buffer)?;
            std::io::stdout().write_all(&buffer)?;
            Ok(ExitCode::SUCCESS)
        }

        Some(Command::Config(command)) => run_config(command),

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

        None => interactive::run(cli, log_notices),
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

    // The same source the interactive program samples, so `snapshot` cannot
    // report a different machine than the interface does (§11.2).
    let mut collector = platform_collector()?;
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::now());
    let mut latest = sample_recording_duration(&mut collector, &tick)?;

    for _ in 1..samples {
        std::thread::sleep(gap);
        tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        latest = sample_recording_duration(&mut collector, &tick)?;
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

/// Samples once, recording the collector's duration in the debug log (§14.2).
///
/// The snapshot subcommand runs a real collector, and it is what people reach for
/// when something is already wrong — so this is exactly the path on which
/// `--debug-log` has to produce the collector timings §14.2 names. Without this the
/// flag would open a file, write one line about opening it, and record nothing about
/// the work that was actually done.
///
/// A failure is logged before it is propagated, because `?` here ends the process:
/// the log is the only place the timing survives.
fn sample_recording_duration(
    collector: &mut impl SnapshotSource,
    tick: &SampleTick,
) -> Result<monitrs_core::SystemSnapshot, monitrs_collectors::CollectorError> {
    let started = Instant::now();
    let outcome = collector.sample(tick);
    let duration = started.elapsed();

    if let Some(tier) = logging::tier_for_pass(tick.due) {
        match &outcome {
            Ok(snapshot) => logging::log_collection(tier, duration, snapshot.process_count()),
            Err(error) => logging::log_collection_failure(tier, duration, error),
        }
    }
    outcome
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

/// `println!` panics when its write fails, which turns a reader closing the pipe
/// into a panic report. Every line this subcommand prints goes through here
/// instead, so the failure arrives as an `io::Error` that [`is_broken_pipe`] can
/// recognise — the same route `snapshot`, `manpage` and `completions` take.
macro_rules! outln {
    ($($arg:tt)*) => {
        writeln!(std::io::stdout(), $($arg)*)?
    };
}

fn run_config(command: &ConfigCommand) -> color_eyre::Result<ExitCode> {
    match command {
        ConfigCommand::Path => {
            let Some(path) = config::default_path() else {
                return Err(color_eyre::eyre::eyre!(
                    "this platform provides no user configuration directory; \
                     use --config <PATH> to name a file explicitly"
                ));
            };
            // Printing whether it exists matters: §12 says monitrs does not create
            // the file, so "not present" is the normal state and should not read
            // as a fault.
            let state = if path.is_file() {
                "present"
            } else {
                "not present"
            };
            outln!("{} ({state})", path.display());
            Ok(ExitCode::SUCCESS)
        }

        ConfigCommand::Init { path } => {
            let target = match path {
                Some(path) => path.clone(),
                None => config::default_path().ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "this platform provides no user configuration directory; \
                         pass a path: monitrs config init <PATH>"
                    )
                })?,
            };
            config::init_file(&target)?;
            outln!("wrote {}", target.display());
            outln!("every value in it is the built-in default, so nothing changed yet");
            Ok(ExitCode::SUCCESS)
        }

        ConfigCommand::Check { path } => {
            let target = match path {
                Some(path) => path.clone(),
                None => match config::default_path() {
                    Some(path) if path.is_file() => path,
                    Some(path) => {
                        outln!(
                            "{} is not present; built-in defaults are valid",
                            path.display()
                        );
                        return Ok(ExitCode::SUCCESS);
                    }
                    None => {
                        return Err(color_eyre::eyre::eyre!(
                            "this platform provides no user configuration directory; \
                             pass a path: monitrs config check <PATH>"
                        ));
                    }
                },
            };
            let (_, warnings) = config::read_and_validate(&target)?;
            outln!("{} is valid", target.display());
            for warning in &warnings {
                outln!("warning: {warning}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_core::SystemSnapshot;
    use monitrs_core::model::{
        MetricState, ProcessIdentity, ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState,
    };
    use std::path::PathBuf;

    /// A temporary directory that removes itself, so no test leaves files behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("monitrs-main-test-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("create temp dir");
            Self(base)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The `snapshot` subcommand as clap would produce it.
    fn snapshot_command() -> Command {
        Command::Snapshot {
            format: SnapshotFormat::Json,
            output: None,
            include_arguments: false,
            samples: 2,
        }
    }

    /// A process carrying the two things §14.2 and §15.2 forbid in a log: a
    /// credential in an argument, and an environment assignment.
    ///
    /// `env NAME=value program` is how a shell passes an environment variable to one
    /// command, so a real process table contains exactly this shape — the value is a
    /// process *argument* as far as any collector can tell, which is why redaction
    /// has to drop the whole argument list rather than look for secrets in it.
    fn process_with_secrets() -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(4_242, 1_700_000),
            parent_pid: Some(1),
            name: "env".into(),
            command: "env DATABASE_PASSWORD=hunter2 psql postgres://admin:hunter2@db.internal/prod"
                .into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Sleeping,
            cpu: MetricState::WarmingUp,
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    /// A snapshot containing that process and nothing else of interest.
    fn snapshot_with_secrets() -> SystemSnapshot {
        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.processes.push(process_with_secrets());
        snapshot
    }

    #[test]
    fn the_debug_log_flag_reaches_settings_on_every_path_not_only_the_interactive_one() {
        // The regression this pins: `--debug-log` used to be read by the interactive
        // runtime alone, so on `monitrs snapshot` it parsed and then did nothing.
        for arguments in [
            vec!["monitrs", "snapshot", "--debug-log", "/tmp/monitrs-cli.log"],
            vec!["monitrs", "--debug-log", "/tmp/monitrs-cli.log", "snapshot"],
            vec![
                "monitrs",
                "config",
                "check",
                "--debug-log",
                "/tmp/monitrs-cli.log",
            ],
        ] {
            let cli = Cli::try_parse_from(&arguments).expect("the flag must parse here");
            let settings = logging::settings_for(
                cli.config.debug_log.as_deref(),
                log_sinks(cli.command.as_ref()),
            );
            assert!(settings.is_enabled(), "{arguments:?} produced no log");
            assert_eq!(
                settings.path.as_deref(),
                Some(std::path::Path::new("/tmp/monitrs-cli.log")),
                "{arguments:?}"
            );
        }

        let interactive = Cli::try_parse_from(["monitrs", "--debug-log", "/tmp/monitrs-cli.log"])
            .expect("parses");
        assert!(
            logging::settings_for(
                interactive.config.debug_log.as_deref(),
                log_sinks(interactive.command.as_ref())
            )
            .is_enabled()
        );
    }

    #[test]
    fn no_flag_means_no_log_on_any_path() {
        for arguments in [
            vec!["monitrs"],
            vec!["monitrs", "snapshot"],
            vec!["monitrs", "config", "path"],
        ] {
            let cli = Cli::try_parse_from(&arguments).expect("parses");
            assert!(
                !logging::settings_for(
                    cli.config.debug_log.as_deref(),
                    log_sinks(cli.command.as_ref())
                )
                .is_enabled(),
                "§14.2: {arguments:?} must default to no log at all"
            );
        }
    }

    #[test]
    fn only_the_path_that_takes_the_screen_is_forbidden_from_printing_the_log() {
        assert!(
            !log_sinks(None).mirrors_stderr(),
            "§14.2: the interactive path must never write to stderr"
        );
        for command in [
            snapshot_command(),
            Command::Manpage,
            Command::Config(ConfigCommand::Path),
            Command::Completions {
                shell: clap_complete::Shell::Bash,
            },
        ] {
            assert!(
                log_sinks(Some(&command)).mirrors_stderr(),
                "a one-shot subcommand has no alternate screen to corrupt: {command:?}"
            );
        }
    }

    #[test]
    fn an_export_run_with_logging_enabled_leaks_no_argument_and_no_environment_value() {
        // §15.2 on the *new* path: the snapshot subcommand now installs a log, so
        // the redaction rules have to hold for what that log records too. The
        // collector is not run — a real process table cannot be asked to contain a
        // known secret — but everything downstream of it is the shipped code.
        let dir = TempDir::new("export-privacy");
        let log_path = dir.file("monitrs.log");
        let export_path = dir.file("snapshot.json");
        let snapshot = snapshot_with_secrets();
        let process = process_with_secrets();

        // Exactly the sinks `monitrs snapshot --debug-log` resolves to, so the test
        // covers the mirror as well as the file.
        let settings = logging::settings_for(
            Some(log_path.as_path()),
            log_sinks(Some(&snapshot_command())),
        );
        let (subscriber, log) = logging::subscriber_for_test(&settings);

        let json = tracing::subscriber::with_default(subscriber, || {
            // What `run_snapshot` does, in the order it does it.
            logging::log_collection(
                logging::tier_for_pass(DueTiers::ALL).expect("every tier is due"),
                Duration::from_millis(12),
                snapshot.process_count(),
            );
            let json = SnapshotExport::new(&snapshot, RedactionPolicy::REDACTED)
                .to_json()
                .expect("the export must serialize");
            std::fs::write(&export_path, json.as_bytes()).expect("the export must be written");
            restrict_to_user(&export_path).expect("permissions must be set");
            // The only entry point that writes a process to the log at all.
            logging::log_process("export", &process);
            json
        });
        // Joins the writer thread, so the file is complete before it is read.
        log.shutdown();

        let logged = std::fs::read_to_string(&log_path).expect("the log file must exist");
        assert!(
            logged.contains("collection completed"),
            "the path must actually log something, or this test proves nothing: {logged}"
        );
        assert!(
            logged.contains("command=\"env\""),
            "the program name is what survives redaction: {logged}"
        );
        for secret in ["hunter2", "DATABASE_PASSWORD", "db.internal", "postgres://"] {
            assert!(
                !logged.contains(secret),
                "§14.2/§15.2: {secret} reached the debug log: {logged}"
            );
            assert!(
                !json.contains(secret),
                "§15.2: {secret} reached the redacted export: {json}"
            );
        }

        // Proof the needle was in the haystack: with arguments explicitly included,
        // the very same snapshot does carry the secret, so the assertions above are
        // about redaction rather than about an empty snapshot.
        let full = SnapshotExport::new(&snapshot, RedactionPolicy::FULL)
            .to_json()
            .expect("the export must serialize");
        assert!(
            full.contains("hunter2"),
            "the fixture must genuinely contain a secret"
        );
    }

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

    /// An error type that merely *carries* a broken pipe as its cause, standing in
    /// for anything monitrs might one day wrap around an inner failure.
    #[derive(Debug)]
    struct Carrying(std::io::Error);

    impl std::fmt::Display for Carrying {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "the collector could not be reached")
        }
    }

    impl std::error::Error for Carrying {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// `tests/broken_pipe.rs` proves the behaviour against a real pipe; these pin the
    /// judgement, in both directions.
    #[test]
    fn the_reader_leaving_is_recognised_through_a_context_line() {
        let bare: color_eyre::Report =
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Broken pipe (os error 32)").into();
        assert!(
            is_broken_pipe(&bare),
            "the error exactly as `write_all` returns it — the shape every stdout \
             write in this binary produces"
        );

        assert!(
            is_broken_pipe(&bare.wrap_err("could not write the export")),
            "`eyre`'s downcast looks through context, so describing the write does \
             not stop the pipe being recognised"
        );
    }

    #[test]
    fn a_broken_pipe_that_is_only_some_other_failure_s_cause_is_still_a_failure() {
        // The narrowness is the safety property: swallowing this would exit 0 and
        // print nothing on a run that really did fail, which is worse than the noisy
        // message this whole change removes.
        let carried: color_eyre::Report = Carrying(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "Broken pipe (os error 32)",
        ))
        .into();
        assert!(
            carried
                .chain()
                .any(|cause| cause.downcast_ref::<std::io::Error>().is_some()),
            "the pipe really is in the chain, so this test would pass vacuously if \
             the fixture were wrong"
        );
        assert!(
            !is_broken_pipe(&carried),
            "it is reachable only as a cause, so it did not come from writing our \
             own stdout and must keep its message and its non-zero exit"
        );
    }

    #[test]
    fn other_write_failures_are_still_failures() {
        // The neighbouring kind most likely to be confused with a closed reader, and
        // the one that must keep its message and its non-zero exit: a full disk while
        // writing `snapshot --output`.
        let full: color_eyre::Report =
            std::io::Error::new(std::io::ErrorKind::StorageFull, "No space left on device").into();
        assert!(!is_broken_pipe(&full));

        let denied: color_eyre::Report =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into();
        assert!(!is_broken_pipe(
            &denied.wrap_err("could not write the export")
        ));

        // An error with no `io::Error` in it at all must not be swallowed either.
        let plain = color_eyre::eyre::eyre!("--samples must be at least 1");
        assert!(!is_broken_pipe(&plain));
    }
}
