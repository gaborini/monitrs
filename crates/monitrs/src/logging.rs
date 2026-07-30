//! The optional debug log (§14.2).
//!
//! # The rule that shapes everything here
//!
//! **Nothing may reach stdout or stderr while the alternate screen is active.**
//! One stray line lands on top of a rendered frame and stays there until the next
//! full redraw, and a log line printed during a panic can scroll the panic report
//! away.
//!
//! That rule is about the *screen*, not about files, and only one of monitrs'
//! execution paths ever takes the screen. So the permitted sinks are a property of
//! the path being run, and the caller has to name it: [`Sinks::FileOnly`] for the
//! interactive runtime, [`Sinks::FileAndStderr`] for a one-shot subcommand such as
//! `monitrs snapshot`, which never enters the alternate screen and whose user is
//! standing at a shell prompt watching for exactly these lines. stdout is never a
//! sink on either path — the snapshot subcommand writes its export there, and a
//! log line in the middle of a JSON document corrupts a different thing just as
//! effectively. Code that needs to talk to the user before the UI is up goes
//! through [`report_problems`], which consults
//! [`monitrs_tui::terminal::alternate_screen_active`] first.
//!
//! # What is on by default
//!
//! Nothing. §14.2 says *default to no file log*, so with no `--debug-log` and no
//! configured path this module installs no subscriber whatsoever — every
//! `tracing` macro in the workspace compiles to a cheap no-op dispatch, no file is
//! created, and the stderr mirror does not exist either. That is also why every
//! `tracing::debug!` elsewhere in the workspace is safe: with no subscriber
//! installed there is nowhere for it to go.
//!
//! # Privacy (§14.2, §15.2)
//!
//! * Command lines are **redacted** through
//!   [`ProcessSnapshot::redacted_command`], because arguments routinely carry
//!   passwords, tokens and connection strings. [`log_process`] is the only way
//!   this module will write a process's command, and it cannot be given the
//!   unredacted form.
//! * Environment variable values are **never** logged. There is no function here
//!   that accepts one, and `ProcessDetail` deliberately does not carry them, so
//!   the value simply does not exist in the process by the time logging could see
//!   it.
//! * The log file is created with user-only permissions where the platform
//!   supports it (§15.2), and pre-existing files are tightened on open — a log
//!   inherits whatever the last run left behind otherwise.
//!
//! # Size (§14.2, §16.1)
//!
//! A debug log writes several lines per sample, so at a one-second interval it
//! grows all day. §14.2 asks for rotating or size-bounded output and §16.1 forbids
//! unbounded growth over a long run, so [`BoundedLogFile`] caps the live file and
//! keeps exactly one previous generation: total on-disk use is bounded at roughly
//! twice [`LogSettings::max_bytes`]. Time-based rotation was rejected because it
//! bounds the *number* of files rather than their size, which is the wrong
//! guarantee for a log whose rate is driven by the sample interval.
//!
//! # Why the writes are non-blocking
//!
//! The sampler and the UI thread both log. A log write that blocks on a slow disk
//! would show up as collector lag or a dropped frame, i.e. the measurement tool
//! would distort the measurement. `tracing_appender::non_blocking` moves the
//! writes to a dedicated thread behind a bounded queue in lossy mode: under
//! pressure it drops log lines and counts them ([`DebugLog::dropped_lines`])
//! rather than stalling a sampling loop.
//!
//! # Drop order matters
//!
//! [`DebugLog`] must be dropped **after** the terminal has been restored.
//! `tracing_appender`'s worker guard prints to stdout if its final flush times
//! out; that is out of our hands, so the mitigation is to make sure stdout is the
//! user's again by then. [`DebugLog::shutdown`] exists to make the order explicit
//! at the call site instead of depending on the order of local variables.

// A few items here are reachable only from the tests below: the size and filter
// builders exist for the configuration hook §14.2 describes, and
// `DebugLog::dropped_lines` for §7.5's report of our own losses. Scoped to
// non-test builds so a genuinely unused item still shows up while the tests are
// running.
#![cfg_attr(not(test), allow(dead_code))]

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use monitrs_collectors::DueTiers;
use monitrs_core::model::{ProcessSnapshot, Tier};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt as _;

/// The default cap on the live log file, per generation.
///
/// 4 MiB is roughly a day of the per-sample debug lines at a one-second interval,
/// and two generations still fit comfortably inside the space a bug report would
/// need.
pub(crate) const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The smallest cap that still leaves room for a whole line.
///
/// A cap below this would rotate on every write and produce two nearly empty
/// files, which loses the information the log exists to carry.
pub(crate) const MIN_MAX_BYTES: u64 = 4 * 1024;

/// How many log lines may queue before the writer starts dropping them.
///
/// Bounded because §10.3 forbids unbounded accumulation anywhere in the pipeline,
/// and this queue is written from the sampler thread.
const BUFFERED_LINES: usize = 4_096;

/// Environment variable that overrides the log filter.
///
/// Only the *verbosity* comes from here. The value is never written into the log:
/// §15.2's rule about environment values exists to protect the machine's secrets,
/// and applying the same restraint to our own environment costs nothing.
pub(crate) const FILTER_ENV: &str = "MONITRS_LOG";

/// The default filter: our own crates at debug, everything else at warn.
///
/// §14.2 requires collector duration and the dropped/coalesced counts at debug
/// level, so debug is the useful default for a log that has been asked for
/// explicitly. Third-party crates stay at warn so their internals cannot bury
/// ours.
pub(crate) const DEFAULT_DIRECTIVES: &str =
    "warn,monitrs=debug,monitrs_core=debug,monitrs_collectors=debug,monitrs_tui=debug";

/// Suffix appended to the log path for the retained previous generation.
const PREVIOUS_SUFFIX: &str = ".1";

/// Paths that name a standard stream rather than a file (§14.2).
///
/// Writing a log to any of these puts log lines on the same file descriptor the
/// UI draws with. They are rejected by name *before* the file is opened, in
/// addition to the `isatty` check that catches `/dev/pts/3` and friends.
const STANDARD_STREAM_PATHS: [&str; 8] = [
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/console",
    "/dev/fd/1",
    "/dev/fd/2",
    "/proc/self/fd/1",
    "/proc/self/fd/2",
];

/// Which sinks a log is allowed to write to.
///
/// The distinction is not a preference, it is §14.2's rule expressed as a type: a
/// log that mirrors to stderr while the alternate screen is active would draw over
/// the interface, so the interactive path must not be able to ask for one by
/// accident. Making the caller name the surface is what keeps that decision
/// visible at the call site instead of buried in a boolean.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Sinks {
    /// The file and nothing else. The only safe choice once a UI owns the screen.
    #[default]
    FileOnly,
    /// The file and stderr, for a path that never enters the alternate screen.
    ///
    /// stdout is deliberately absent: `monitrs snapshot` writes its export there.
    FileAndStderr,
}

impl Sinks {
    /// Whether stderr receives a copy of every recorded line.
    #[must_use]
    pub(crate) const fn mirrors_stderr(self) -> bool {
        matches!(self, Self::FileAndStderr)
    }
}

/// Where the debug log goes and how large it may grow.
///
/// `path` is `None` for the default, which is *no log at all* (§14.2). The
/// runtime fills this in from `--debug-log` or from the configuration file,
/// whichever won the §12 merge; this module deliberately does not know which,
/// because the rules it enforces are identical either way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LogSettings {
    /// The file to append to, or `None` for no logging.
    pub(crate) path: Option<PathBuf>,
    /// Cap on the live file. Clamped to at least [`MIN_MAX_BYTES`] on open.
    pub(crate) max_bytes: u64,
    /// Filter directives, overriding [`DEFAULT_DIRECTIVES`] and [`FILTER_ENV`].
    pub(crate) directives: Option<String>,
    /// Which sinks the recorded lines may reach.
    pub(crate) sinks: Sinks,
}

impl Default for LogSettings {
    /// No log, which is what §14.2 requires of a default configuration.
    ///
    /// The default sink set is the restrictive one, so a caller that forgets to
    /// think about the alternate screen cannot accidentally print over it.
    fn default() -> Self {
        Self {
            path: None,
            max_bytes: DEFAULT_MAX_BYTES,
            directives: None,
            sinks: Sinks::FileOnly,
        }
    }
}

impl LogSettings {
    /// Settings that log nowhere.
    #[must_use]
    pub(crate) fn disabled() -> Self {
        Self::default()
    }

    /// Settings that append to `path`.
    #[must_use]
    pub(crate) fn to_file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            ..Self::default()
        }
    }

    /// These settings, restricted to the given sinks.
    #[must_use]
    pub(crate) const fn with_sinks(mut self, sinks: Sinks) -> Self {
        self.sinks = sinks;
        self
    }

    /// These settings with a different size cap.
    #[must_use]
    pub(crate) fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// These settings with explicit filter directives.
    #[must_use]
    pub(crate) fn with_directives(mut self, directives: impl Into<String>) -> Self {
        self.directives = Some(directives.into());
        self
    }

    /// Whether a log was asked for at all.
    #[must_use]
    pub(crate) fn is_enabled(&self) -> bool {
        self.path.is_some()
    }
}

/// The settings implied by `--debug-log` on a given execution path.
///
/// The one place that turns the flag into settings, so `--debug-log` cannot mean
/// one thing to the interactive runtime and something else — or nothing at all — to
/// a subcommand. `None` is the documented default of no log whatsoever (§14.2), and
/// it stays that way regardless of `sinks`: a path that *may* print is still not a
/// path that logs unasked.
#[must_use]
pub(crate) fn settings_for(debug_log: Option<&Path>, sinks: Sinks) -> LogSettings {
    match debug_log {
        Some(path) => LogSettings::to_file(path).with_sinks(sinks),
        None => LogSettings::disabled(),
    }
}

/// Why a debug log could not be opened.
///
/// Every variant is recoverable: §14.1 separates fatal startup errors from the
/// rest, and a log that cannot be written is emphatically not a reason to refuse
/// to show the system's CPU usage.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LogError {
    /// The path was empty or otherwise unusable as a file name.
    #[error("a debug log path must not be empty")]
    EmptyPath,

    /// The path names a terminal or a standard stream (§14.2).
    #[error(
        "{path} is a terminal or standard stream; logging there would overwrite the interface, \
         so no log was started"
    )]
    WouldCorruptDisplay {
        /// The offending path.
        path: PathBuf,
    },

    /// The parent directory could not be created.
    #[error("could not create the directory for the debug log at {path}: {source}")]
    Directory {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The file could not be opened for appending.
    #[error("could not open the debug log at {path}: {source}")]
    Open {
        /// The path that could not be opened.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The path exists but is not a regular file.
    ///
    /// A separate variant because the *reason* matters: opening a named pipe with
    /// no reader blocks, which would hang startup before anything could be
    /// printed, and a device path is the display-corruption hazard again.
    #[error("{path} is a {kind}, not a regular file, so it cannot hold the debug log")]
    NotARegularFile {
        /// The offending path.
        path: PathBuf,
        /// What it actually is.
        kind: &'static str,
    },

    /// Something already installed a global subscriber.
    #[error(
        "a log subscriber is already installed for this process, so {path} was not opened: {source}"
    )]
    AlreadyInstalled {
        /// The path that was going to be used.
        path: PathBuf,
        /// Why the installation was refused.
        #[source]
        source: tracing::subscriber::SetGlobalDefaultError,
    },
}

/// A size-bounded append-only log file that keeps one previous generation.
///
/// The bound is enforced at line granularity: a write that would cross the cap
/// rotates first, so the live file can exceed `max_bytes` by at most the length
/// of one log line. Rotating before rather than after the write is what keeps the
/// *live* file — the one a user tails — under the cap.
#[derive(Debug)]
struct BoundedLogFile {
    path: PathBuf,
    previous: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
}

impl BoundedLogFile {
    /// Opens `path` for appending, creating it (and its parent) if needed.
    fn open(path: &Path, max_bytes: u64) -> Result<Self, LogError> {
        if path.as_os_str().is_empty() {
            return Err(LogError::EmptyPath);
        }
        if names_a_standard_stream(path) {
            return Err(LogError::WouldCorruptDisplay {
                path: path.to_path_buf(),
            });
        }
        // Refuse anything that exists and is not a regular file, *before* opening
        // it. Two reasons, both about startup: opening a named pipe with no reader
        // blocks indefinitely, which would hang monitrs with nothing on screen to
        // explain why; and a device, socket or directory is not a log sink. This
        // follows symlinks, which is deliberate — a symlink pointing at the user's
        // terminal is exactly the case §14.2 is about.
        if let Ok(metadata) = std::fs::metadata(path)
            && !metadata.is_file()
        {
            return Err(LogError::NotARegularFile {
                path: path.to_path_buf(),
                kind: describe_kind(&metadata),
            });
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| LogError::Directory {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let file = open_append(path).map_err(|source| LogError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        // Belt and braces for the two checks above: `isatty` on the handle we
        // actually hold is the authoritative answer, it needs no unsafe code, and
        // it still holds if the path was replaced between the check and the open.
        if file.is_terminal() {
            return Err(LogError::WouldCorruptDisplay {
                path: path.to_path_buf(),
            });
        }
        // Mode flags only apply to a file this call created, so an existing log
        // from an earlier run is tightened explicitly (§15.2).
        restrict_to_user(path).map_err(|source| LogError::Open {
            path: path.to_path_buf(),
            source,
        })?;

        let written = file.metadata().map_or(0, |metadata| metadata.len());
        let mut previous = path.as_os_str().to_owned();
        previous.push(PREVIOUS_SUFFIX);

        Ok(Self {
            path: path.to_path_buf(),
            previous: PathBuf::from(previous),
            file,
            written,
            max_bytes: max_bytes.max(MIN_MAX_BYTES),
        })
    }

    /// Moves the live file aside and starts a fresh one.
    ///
    /// If the rename fails the live file is truncated instead: §16.1's bound on
    /// growth has to hold even on a filesystem that will not rename, and losing
    /// old log lines is a smaller failure than filling the disk.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        match std::fs::rename(&self.path, &self.previous) {
            Ok(()) => {
                self.file = open_append(&self.path)?;
                let _ = restrict_to_user(&self.path);
            }
            Err(_) => {
                self.file.set_len(0)?;
            }
        }
        self.written = 0;
        Ok(())
    }
}

impl Write for BoundedLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        // `self.written > 0` keeps a single line larger than the whole cap from
        // rotating an empty file forever.
        if self.written > 0 && self.written.saturating_add(len) > self.max_bytes {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Opens a file for appending, with user-only permissions where supported.
#[cfg(unix)]
fn open_append(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Opens a file for appending. Non-Unix platforms are not v1 targets, and
/// pretending to set permissions there would be dishonest (§15.2).
#[cfg(not(unix))]
fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Restricts an existing file to the current user (§15.2).
#[cfg(unix)]
fn restrict_to_user(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Non-Unix platforms expose no equivalent, so nothing is claimed.
#[cfg(not(unix))]
fn restrict_to_user(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Names what a non-regular file is, so the message can say why it was refused.
fn describe_kind(metadata: &std::fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        return "directory";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        let kind = metadata.file_type();
        if kind.is_fifo() {
            return "named pipe";
        }
        if kind.is_socket() {
            return "socket";
        }
        if kind.is_char_device() {
            return "character device";
        }
        if kind.is_block_device() {
            return "block device";
        }
    }
    "special file"
}

/// Whether `path` names one of the standard streams or a controlling terminal.
///
/// A pure predicate so the table can be tested without a terminal, which §14.3's
/// testability rule asks of anything terminal-shaped.
#[must_use]
pub(crate) fn names_a_standard_stream(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|text| STANDARD_STREAM_PATHS.contains(&text))
}

/// A live debug log.
///
/// Keeping this alive is what keeps the log open: dropping it flushes the queue
/// and joins the writer thread. Drop it **after** restoring the terminal (see the
/// module documentation) or call [`shutdown`](Self::shutdown) explicitly.
#[derive(Debug)]
#[must_use = "dropping the log immediately closes it again"]
pub(crate) struct DebugLog {
    path: PathBuf,
    max_bytes: u64,
    sinks: Sinks,
    dropped: ErrorCounter,
    guard: Option<WorkerGuard>,
}

impl DebugLog {
    /// The file being appended to.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The cap in effect on the live file.
    #[must_use]
    pub(crate) const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Whether every recorded line is also going to stderr.
    ///
    /// Recorded in the log's own opening line, so a file someone sends in a bug
    /// report says whether its contents were also on their screen.
    #[must_use]
    pub(crate) const fn mirrors_stderr(&self) -> bool {
        self.sinks.mirrors_stderr()
    }

    /// How many log lines the bounded queue had to drop.
    ///
    /// Non-zero means logging itself was under pressure, which the Inspect screen
    /// can show beside the collector's own dropped-sample count: a monitor that
    /// hides its own losses is lying (§7.5).
    #[must_use]
    pub(crate) fn dropped_lines(&self) -> usize {
        self.dropped.dropped_lines()
    }

    /// Flushes and closes the log at a point the caller chooses.
    pub(crate) fn shutdown(mut self) {
        drop(self.guard.take());
    }
}

/// The outcome of setting logging up.
///
/// Never an `Err`: §14.2 says a failure to open the log must not stop monitrs
/// from starting, so problems are carried alongside whatever did succeed and
/// reported through [`report_problems`].
#[derive(Debug)]
pub(crate) struct LogStartup {
    /// The live log, or `None` when logging is off or could not be started.
    pub(crate) log: Option<DebugLog>,
    /// Human-readable problems, in the order they were found.
    pub(crate) problems: Vec<String>,
}

impl LogStartup {
    /// Whether a log is actually running.
    #[must_use]
    pub(crate) fn is_logging(&self) -> bool {
        self.log.is_some()
    }
}

/// Builds the writer and the subscriber for `settings` without installing it.
///
/// Split out from [`install`] because a process may only install one global
/// subscriber, and the tests need to build many. Tests drive the returned
/// subscriber with `tracing::subscriber::with_default`.
///
/// Settings with no path are a caller error here — [`install`] answers that case
/// by installing nothing at all — and are reported as [`LogError::EmptyPath`].
fn build(
    settings: &LogSettings,
    environment: Option<&str>,
) -> Result<
    (
        impl tracing::Subscriber + Send + Sync + 'static,
        DebugLog,
        Vec<String>,
    ),
    LogError,
> {
    let path = settings.path.clone().ok_or(LogError::EmptyPath)?;
    let sink = BoundedLogFile::open(&path, settings.max_bytes)?;
    let max_bytes = sink.max_bytes;

    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(BUFFERED_LINES)
        // Lossy: a full queue drops log lines and counts them rather than
        // blocking the thread that produced them (§16.2 — never grow queues
        // without bound, never stall the pipeline).
        .lossy(true)
        .thread_name("monitrs-log")
        .finish(sink);

    let (filter, problems) = build_filter(settings.directives.as_deref(), environment);
    let dropped = writer.error_counter();
    let subscriber = subscriber_for(writer, filter, settings.sinks);

    Ok((
        subscriber,
        DebugLog {
            path,
            max_bytes,
            sinks: settings.sinks,
            dropped,
            guard: Some(guard),
        },
        problems,
    ))
}

/// The subscriber layout: one filter, one file sink, optionally a stderr mirror.
///
/// `with_ansi(false)` is not cosmetic. Escape sequences in a log file make it
/// unreadable in a bug report, and a file that is later `cat`-ed into a terminal
/// would replay colour changes. The mirror is colourless for the same reason a
/// pipe should be: its output is as likely to be redirected into a file as read.
///
/// Both layers format the *same* event, which is what makes the privacy rules
/// sink-independent: redaction happens at the call site (see [`log_process`]), so
/// there is nothing in the event for a second sink to reveal.
fn subscriber_for(
    writer: NonBlocking,
    filter: EnvFilter,
    sinks: Sinks,
) -> impl tracing::Subscriber + Send + Sync + 'static {
    // A blocking writer, unlike the file sink. Justified because the mirror only
    // exists on paths with no render loop and no sampler to distort (§16.2), and
    // because a mirror that dropped lines while the file kept them would make the
    // two disagree in exactly the situation someone is reading both.
    let mirror = sinks.mirrors_stderr().then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_thread_names(true)
            .with_writer(io::stderr)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_level(true)
                // Which thread produced a line is most of the value of this log:
                // §10.3's four threads each have their own failure modes.
                .with_thread_names(true)
                .with_writer(writer),
        )
        .with(mirror)
}

/// Resolves the filter, preferring an explicit setting over the environment.
///
/// An unparsable directive falls back to the default rather than disabling
/// logging: the user asked for a log, and refusing to produce one because a
/// filter string had a typo would be the least helpful possible response.
fn build_filter(explicit: Option<&str>, environment: Option<&str>) -> (EnvFilter, Vec<String>) {
    for (source, candidate) in [
        ("the configured log filter", explicit),
        (FILTER_ENV, environment),
    ] {
        let Some(text) = candidate.map(str::trim).filter(|text| !text.is_empty()) else {
            continue;
        };
        return match EnvFilter::try_new(text) {
            Ok(filter) => (filter, Vec::new()),
            Err(error) => (
                default_filter(),
                vec![format!(
                    "{source} could not be parsed ({error}); the default log filter is in use"
                )],
            ),
        };
    }
    (default_filter(), Vec::new())
}

/// [`DEFAULT_DIRECTIVES`] as a filter, with a panic-free fallback.
///
/// `EnvFilter::new` panics on a bad directive and §14.3 forbids panicking, so the
/// constant is parsed fallibly even though a test pins it as valid.
fn default_filter() -> EnvFilter {
    EnvFilter::try_new(DEFAULT_DIRECTIVES)
        .unwrap_or_else(|_| EnvFilter::default().add_directive(LevelFilter::DEBUG.into()))
}

/// Installs the debug log described by `settings`, or nothing at all.
///
/// Returns the problems it hit rather than failing: §14.2's "a failure to open
/// the log must not prevent monitrs from starting". With no path configured this
/// installs **no** subscriber, so `tracing` events go nowhere — which is exactly
/// what §14.2's "default to no file log" means for a program that must not print.
pub(crate) fn install(settings: &LogSettings) -> LogStartup {
    if !settings.is_enabled() {
        return LogStartup {
            log: None,
            problems: Vec::new(),
        };
    }

    let environment = std::env::var(FILTER_ENV).ok();
    match build(settings, environment.as_deref()) {
        Ok((subscriber, log, mut problems)) => {
            if let Err(source) = tracing::subscriber::set_global_default(subscriber) {
                let path = log.path().to_path_buf();
                // Dropping the log closes the file we just opened; the alternative
                // would be a live writer thread nothing can reach.
                drop(log);
                problems.push(LogError::AlreadyInstalled { path, source }.to_string());
                return LogStartup {
                    log: None,
                    problems,
                };
            }
            tracing::info!(
                target: TARGET_LOGGING,
                path = %log.path().display(),
                max_bytes = log.max_bytes(),
                stderr_mirror = log.mirrors_stderr(),
                "debug log started; command lines are redacted and environment values are never logged"
            );
            LogStartup {
                log: Some(log),
                problems,
            }
        }
        Err(error) => LogStartup {
            log: None,
            problems: vec![error.to_string()],
        },
    }
}

/// Where a startup problem may be written right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProblemDestination {
    /// stderr is still the user's, so a message printed there is visible.
    Stderr,
    /// The UI owns the screen; the message must wait for a notice or the log.
    Deferred,
}

/// Decides where a problem may be reported (§14.2).
///
/// A pure function of the one fact that matters, so the decision is testable
/// without a terminal.
#[must_use]
pub(crate) const fn destination(alternate_screen_active: bool) -> ProblemDestination {
    if alternate_screen_active {
        ProblemDestination::Deferred
    } else {
        ProblemDestination::Stderr
    }
}

/// Reports startup problems, returning the ones that could not be printed.
///
/// Before the UI starts, problems go to stderr where the user will see them.
/// Once the alternate screen is active nothing may be printed, so the messages
/// are logged (to the file, or nowhere) and handed back for the caller to show
/// as an in-UI notice.
pub(crate) fn report_problems(problems: &[String]) -> Vec<String> {
    match destination(monitrs_tui::terminal::alternate_screen_active()) {
        ProblemDestination::Stderr => {
            for problem in problems {
                eprintln!("monitrs: {problem}");
            }
            Vec::new()
        }
        ProblemDestination::Deferred => {
            for problem in problems {
                tracing::warn!(
                    target: TARGET_LOGGING,
                    %problem,
                    "logging problem could not be printed"
                );
            }
            problems.to_vec()
        }
    }
}

/// Milliseconds, saturating, without a float cast.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The filter target of the collection lines §14.2 asks for.
///
/// Every recorder below names its target explicitly rather than inheriting
/// `tracing`'s default, which is the *module path of the macro call* — that is,
/// `logging`, wherever the work actually happened. Two reasons, and the second one
/// bites:
///
/// * A reader filtering on `monitrs::collection` wants collections, not "lines that
///   happen to be emitted from the logging module".
/// * The default target carries the crate name of whatever is being linked.
///   `tests/soak.rs` compiles this module into a test binary by path, where the
///   default target becomes `soak::logging` and [`DEFAULT_DIRECTIVES`]' `monitrs`
///   entry stops matching — so the same code would log in the binary and log
///   nothing there. An explicit target is the same string in every build.
const TARGET_COLLECTION: &str = "monitrs::collection";

/// The filter target of the sample-channel accounting lines.
const TARGET_CHANNEL: &str = "monitrs::channel";

/// The filter target of the per-process lines.
const TARGET_PROCESS: &str = "monitrs::process";

/// The filter target of the log's own lifecycle and problems.
const TARGET_LOGGING: &str = "monitrs::logging";

/// Which tier one sampling pass should be recorded under, if any.
///
/// §8.6 deliberately collects every due tier in a single pass so that the snapshot
/// is internally consistent, which leaves one measured duration covering two or
/// three tiers' work. Attributing it to the *coarsest* tier present is what keeps
/// the numbers in the log comparable: a fast-only pass and a pass that also re-read
/// filesystem capacity and the device list are different kinds of work, and
/// averaging them under one label would hide a regression in either.
///
/// `None` when nothing was due, because then no collection happened and there is no
/// duration to attribute. The sampler never asks in that case; returning a tier
/// anyway would put an invented measurement in the log.
#[must_use]
pub(crate) fn tier_for_pass(due: DueTiers) -> Option<Tier> {
    [Tier::Slow, Tier::Medium, Tier::Fast]
        .into_iter()
        .find(|tier| due.contains(*tier))
}

/// Records one completed collection (§14.2: collector duration at debug level).
pub(crate) fn log_collection(tier: Tier, duration: Duration, processes: usize) {
    tracing::debug!(
        target: TARGET_COLLECTION,
        tier = tier.label(),
        duration_ms = millis(duration),
        processes,
        "collection completed"
    );
}

/// Records a failed collection.
///
/// `debug` rather than `warn`: §14.1 classifies a recoverable collector error as
/// ordinary operation, and a partially unavailable source would otherwise emit a
/// warning every second.
pub(crate) fn log_collection_failure(
    tier: Tier,
    duration: Duration,
    error: &dyn std::fmt::Display,
) {
    tracing::debug!(
        target: TARGET_COLLECTION,
        tier = tier.label(),
        duration_ms = millis(duration),
        %error,
        "collection failed"
    );
}

/// Records the sample channel's losses (§14.2: dropped/coalesced counts at debug
/// level).
pub(crate) fn log_channel(dropped: u64, coalesced: u64, lag: Duration) {
    tracing::debug!(
        target: TARGET_CHANNEL,
        dropped,
        coalesced,
        lag_ms = millis(lag),
        "sample channel accounting"
    );
}

/// Records a process, with its command line redacted (§14.2, §15.2).
///
/// The only process-logging entry point in the program, and it takes the whole
/// snapshot rather than a string so that a caller *cannot* pass the unredacted
/// command by mistake. Arguments carry credentials — `psql
/// postgres://user:secret@host/db` is an ordinary command line — so only
/// `argv[0]` is written. Environment values are absent from `ProcessSnapshot`
/// entirely, so there is nothing here that could leak one.
///
/// There is deliberately **no** opt-in to logging the full command line, unlike
/// `monitrs snapshot --include-arguments`. The difference is what happens to the
/// output: an export is a deliberate, one-off act, while a debug log accumulates
/// for hours and is the file people attach to a bug report without reading it.
pub(crate) fn log_process(context: &'static str, process: &ProcessSnapshot) {
    tracing::debug!(
        target: TARGET_PROCESS,
        pid = process.identity.pid,
        start_key = process.identity.start_key,
        name = %process.name,
        command = process.redacted_command(),
        context,
        "process"
    );
}

/// Builds a file-backed subscriber for a test, without installing it globally.
///
/// A process may install exactly one global subscriber, so a test that wants to
/// read back what was written cannot go through [`install`] — it would have to be
/// the first test to run. `tracing::subscriber::with_default` is the alternative,
/// and this is how a test outside this module gets something to hand it.
///
/// Test-only on purpose: production code must go through [`install`], which is
/// where §14.2's refusals and the "never fatal" contract live.
#[cfg(test)]
pub(crate) fn subscriber_for_test(
    settings: &LogSettings,
) -> (impl tracing::Subscriber + Send + Sync + 'static, DebugLog) {
    let (subscriber, log, problems) = build(settings, None).expect("the test log must open");
    assert!(problems.is_empty(), "{problems:?}");
    (subscriber, log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_core::model::{
        MetricState, ProcessIdentity, ProcessIo, ProcessMemory, ProcessState,
    };

    /// A temporary directory that removes itself, so no test leaves files behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "monitrs-logging-test-{label}-{}",
                std::process::id()
            ));
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

    fn process_with_command(command: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(4_242, 1_700_000),
            parent_pid: Some(1),
            name: "psql".into(),
            command: command.into(),
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

    /// Runs `body` with a file-backed subscriber, then flushes and reads the file.
    ///
    /// `with_default` rather than `install`: a process can only install one global
    /// subscriber, so every test would have to be the first one to run.
    fn logged_to_file(settings: &LogSettings, body: impl FnOnce()) -> String {
        let path = settings.path.clone().expect("a path");
        let (subscriber, log) = subscriber_for_test(settings);
        tracing::subscriber::with_default(subscriber, body);
        // Dropping the log joins the writer thread, so the file is complete.
        drop(log);
        std::fs::read_to_string(&path).expect("the log file is readable")
    }

    #[test]
    fn logging_is_off_unless_a_path_is_given() {
        let settings = LogSettings::disabled();
        assert_eq!(settings, LogSettings::default(), "§14.2: no log by default");
        assert!(!settings.is_enabled());
        let startup = install(&settings);
        assert!(!startup.is_logging());
        assert!(startup.problems.is_empty());
    }

    #[test]
    fn a_path_that_may_print_still_logs_nothing_unless_a_log_was_asked_for() {
        // The permission to mirror to stderr is not a reason to start logging:
        // §14.2's default is no log at all, on every path.
        let settings = settings_for(None, Sinks::FileAndStderr);
        assert_eq!(settings, LogSettings::disabled());
        assert!(!settings.is_enabled());
        let startup = install(&settings);
        assert!(!startup.is_logging(), "no log means no sink at all");
        assert!(startup.problems.is_empty());
    }

    #[test]
    fn the_flag_turns_into_the_same_file_settings_on_every_path() {
        let path = Path::new("/tmp/monitrs-settings-for.log");
        let interactive = settings_for(Some(path), Sinks::FileOnly);
        let one_shot = settings_for(Some(path), Sinks::FileAndStderr);

        assert_eq!(interactive.path.as_deref(), Some(path));
        assert_eq!(one_shot.path.as_deref(), Some(path));
        assert_eq!(interactive.max_bytes, one_shot.max_bytes);
        assert!(
            !interactive.sinks.mirrors_stderr(),
            "§14.2: the interactive path must never print"
        );
        assert!(one_shot.sinks.mirrors_stderr());
        assert_eq!(
            LogSettings::default().sinks,
            Sinks::FileOnly,
            "the restrictive sink set has to be the default"
        );
    }

    #[test]
    fn a_running_log_reports_which_sinks_it_ended_up_with() {
        // Written into the log's own opening line, so a file in a bug report says
        // whether its contents were also on the reporter's screen.
        let dir = TempDir::new("sink-report");
        let (mirrored, mirrored_log) = subscriber_for_test(
            &LogSettings::to_file(dir.file("mirrored.log")).with_sinks(Sinks::FileAndStderr),
        );
        assert!(mirrored_log.mirrors_stderr());
        drop(mirrored);
        mirrored_log.shutdown();

        let (quiet, quiet_log) = subscriber_for_test(&LogSettings::to_file(dir.file("quiet.log")));
        assert!(
            !quiet_log.mirrors_stderr(),
            "§14.2: the default must stay the silent one"
        );
        drop(quiet);
        quiet_log.shutdown();
    }

    #[test]
    fn a_log_that_could_not_be_opened_is_reported_and_nothing_starts() {
        // §14.2: failing to open the log must not stop monitrs, and it must not
        // leave a half-live log behind either.
        let dir = TempDir::new("mirror-failed");
        let settings = settings_for(Some(&dir.0), Sinks::FileAndStderr);
        let startup = install(&settings);
        assert!(!startup.is_logging(), "a directory is not a log file");
        assert_eq!(startup.problems.len(), 1, "{:?}", startup.problems);
    }

    #[test]
    fn a_mirrored_log_still_redacts_the_command_in_the_file() {
        // The mirror is a second sink for the *same* event, so redaction cannot
        // depend on which sinks are enabled. Pinned rather than assumed.
        let dir = TempDir::new("mirror-redaction");
        let settings = LogSettings::to_file(dir.file("monitrs.log"))
            .with_sinks(Sinks::FileAndStderr)
            // One filter serves both layers, so this test necessarily puts its one
            // line on the test runner's stderr as well. That is the behaviour under
            // test; there is no way to enable the mirror and see nothing from it.
            .with_directives("monitrs=debug");
        let process = process_with_command("psql postgres://admin:hunter2@db.internal/prod");
        let contents = logged_to_file(&settings, || log_process("selection", &process));
        assert!(contents.contains("command=\"psql\""), "{contents}");
        assert!(!contents.contains("hunter2"), "{contents}");
    }

    #[test]
    fn a_credential_in_a_command_line_never_reaches_the_log_file() {
        let dir = TempDir::new("redaction");
        let settings = LogSettings::to_file(dir.file("monitrs.log"));
        let process = process_with_command("psql postgres://admin:hunter2@db.internal/prod");

        let contents = logged_to_file(&settings, || {
            log_process("selection", &process);
        });

        assert!(
            contents.contains("psql"),
            "the program name is the point of the line: {contents}"
        );
        assert!(
            !contents.contains("hunter2"),
            "§14.2: arguments may contain secrets and must be redacted: {contents}"
        );
        assert!(
            !contents.contains("db.internal"),
            "the whole argument list is dropped, not just the password: {contents}"
        );
        assert!(
            contents.contains("pid=4242"),
            "the identity must survive redaction: {contents}"
        );
    }

    #[test]
    fn a_secret_carried_in_a_flag_is_dropped_with_the_rest_of_the_argument_list() {
        // Redaction keeps argv[0] and nothing else, so it does not matter which
        // argument the secret hid in.
        let dir = TempDir::new("redaction-flag");
        let settings = LogSettings::to_file(dir.file("monitrs.log"));
        let process = process_with_command("aws --profile secret-profile s3 ls");
        let contents = logged_to_file(&settings, || log_process("test", &process));
        assert!(contents.contains("command=\"aws\""), "{contents}");
        assert!(!contents.contains("secret-profile"), "{contents}");
    }

    #[test]
    fn collector_duration_and_dropped_counts_are_logged_at_debug_level() {
        let dir = TempDir::new("health");
        let settings = LogSettings::to_file(dir.file("monitrs.log"));
        let contents = logged_to_file(&settings, || {
            log_collection(Tier::Fast, Duration::from_millis(37), 214);
            log_channel(3, 11, Duration::from_millis(1_400));
        });

        assert!(contents.contains("DEBUG"), "{contents}");
        assert!(contents.contains("duration_ms=37"), "{contents}");
        assert!(contents.contains("processes=214"), "{contents}");
        assert!(contents.contains("tier=\"fast\""), "{contents}");
        assert!(contents.contains("dropped=3"), "{contents}");
        assert!(contents.contains("coalesced=11"), "{contents}");
        assert!(contents.contains("lag_ms=1400"), "{contents}");
    }

    #[test]
    fn a_pass_is_attributed_to_the_coarsest_tier_it_refreshed() {
        use monitrs_collectors::{TierIntervals, TierScheduler};
        use std::time::Instant;

        assert_eq!(
            tier_for_pass(DueTiers::ALL),
            Some(Tier::Slow),
            "the first pass refreshes everything, and the slow tier is what made it cost"
        );
        assert_eq!(
            tier_for_pass(DueTiers::NONE),
            None,
            "nothing was collected, so there is no duration to attribute"
        );

        // The ordinary case, taken from the real scheduler rather than assembled by
        // hand: once every tier has run, only the fast one comes due again.
        let mut scheduler = TierScheduler::new(TierIntervals {
            fast: Duration::from_millis(1),
            medium: Duration::from_secs(600),
            slow: Duration::from_secs(600),
        });
        let start = Instant::now();
        scheduler.mark_completed(DueTiers::ALL, start);
        let due = scheduler.due_at(start + Duration::from_millis(50));
        assert_eq!(tier_for_pass(due), Some(Tier::Fast));
    }

    #[test]
    fn a_recoverable_collector_error_is_debug_not_warn() {
        let dir = TempDir::new("collector-error");
        let settings = LogSettings::to_file(dir.file("monitrs.log"));
        let contents = logged_to_file(&settings, || {
            log_collection_failure(
                Tier::Medium,
                Duration::from_millis(4),
                &"/proc/diskstats: read failed",
            );
        });
        assert!(contents.contains("collection failed"), "{contents}");
        assert!(
            !contents.contains("WARN"),
            "§14.1: a recoverable collector error is not a warning: {contents}"
        );
    }

    #[test]
    fn the_log_file_carries_no_escape_sequences() {
        let dir = TempDir::new("ansi");
        let settings = LogSettings::to_file(dir.file("monitrs.log"));
        let contents = logged_to_file(&settings, || {
            log_collection(Tier::Fast, Duration::from_millis(1), 1);
        });
        assert!(
            !contents.contains('\u{1b}'),
            "a log file must be readable in a bug report: {contents:?}"
        );
    }

    #[test]
    fn the_live_file_stays_inside_its_size_cap_and_one_generation_is_kept() {
        let dir = TempDir::new("rotation");
        let path = dir.file("monitrs.log");
        let mut sink = BoundedLogFile::open(&path, MIN_MAX_BYTES).expect("opens");
        let line = [b'x'; 256];

        for _ in 0..200 {
            sink.write_all(&line).expect("writes");
        }
        sink.flush().expect("flushes");
        drop(sink);

        let live = std::fs::metadata(&path).expect("live file").len();
        assert!(
            live <= MIN_MAX_BYTES,
            "the live file must stay inside the cap, got {live}"
        );

        let previous = PathBuf::from(format!("{}{PREVIOUS_SUFFIX}", path.display()));
        let retained = std::fs::metadata(&previous)
            .expect("one generation is kept")
            .len();
        assert!(
            retained <= MIN_MAX_BYTES + u64::try_from(line.len()).expect("small"),
            "the retained generation is bounded too, got {retained}"
        );
        assert_eq!(
            std::fs::read_dir(&dir.0).expect("readable").count(),
            2,
            "exactly the live file and one previous generation"
        );
    }

    #[test]
    fn a_line_larger_than_the_whole_cap_is_written_rather_than_looping() {
        let dir = TempDir::new("huge-line");
        let path = dir.file("monitrs.log");
        let mut sink = BoundedLogFile::open(&path, MIN_MAX_BYTES).expect("opens");
        let huge = vec![b'y'; usize::try_from(MIN_MAX_BYTES).expect("small") * 3];
        sink.write_all(&huge).expect("writes");
        sink.flush().expect("flushes");
        drop(sink);
        assert_eq!(
            std::fs::metadata(&path).expect("live file").len(),
            u64::try_from(huge.len()).expect("small"),
            "one oversized line is kept whole; the alternative is an endless rotate loop"
        );
    }

    #[test]
    fn a_cap_below_the_floor_is_raised_rather_than_honoured() {
        let dir = TempDir::new("tiny-cap");
        let settings = LogSettings::to_file(dir.file("monitrs.log")).with_max_bytes(1);
        let (subscriber, log, _) = build(&settings, None).expect("opens");
        assert_eq!(log.max_bytes(), MIN_MAX_BYTES);
        drop(subscriber);
        log.shutdown();
    }

    #[test]
    fn reopening_an_existing_log_appends_rather_than_truncating() {
        let dir = TempDir::new("append");
        let path = dir.file("monitrs.log");
        std::fs::write(&path, b"earlier run\n").expect("seed");

        let settings = LogSettings::to_file(&path);
        let contents = logged_to_file(&settings, || {
            log_collection(Tier::Slow, Duration::from_millis(9), 3);
        });
        assert!(
            contents.starts_with("earlier run\n"),
            "an earlier run's log must not be destroyed: {contents}"
        );
        assert!(contents.contains("duration_ms=9"), "{contents}");
    }

    #[cfg(unix)]
    #[test]
    fn the_log_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new("perms");
        let path = dir.file("monitrs.log");
        let sink = BoundedLogFile::open(&path, DEFAULT_MAX_BYTES).expect("opens");
        drop(sink);
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "§15.2: logs use user-only permissions");
    }

    #[cfg(unix)]
    #[test]
    fn a_log_left_world_readable_by_an_earlier_run_is_tightened_on_open() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new("perms-existing");
        let path = dir.file("monitrs.log");
        std::fs::write(&path, b"earlier run\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let sink = BoundedLogFile::open(&path, DEFAULT_MAX_BYTES).expect("opens");
        drop(sink);
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a mode flag only applies to a created file");
    }

    #[test]
    fn a_standard_stream_path_is_refused_before_it_can_corrupt_the_display() {
        for name in STANDARD_STREAM_PATHS {
            assert!(
                names_a_standard_stream(Path::new(name)),
                "{name} must be refused"
            );
            let error =
                BoundedLogFile::open(Path::new(name), DEFAULT_MAX_BYTES).expect_err("refused");
            assert!(
                matches!(error, LogError::WouldCorruptDisplay { .. }),
                "{name}: {error}"
            );
        }
        assert!(!names_a_standard_stream(Path::new("/tmp/monitrs.log")));
        assert!(!names_a_standard_stream(Path::new("/dev/shm/monitrs.log")));
    }

    #[test]
    fn a_directory_is_refused_with_a_message_that_says_what_it_is() {
        let dir = TempDir::new("directory-sink");
        let error = BoundedLogFile::open(&dir.0, DEFAULT_MAX_BYTES).expect_err("refused");
        assert!(
            matches!(
                error,
                LogError::NotARegularFile {
                    kind: "directory",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_a_regular_file_is_refused_before_it_can_block_startup() {
        // A socket stands in for the whole class, a named pipe being the dangerous
        // member: opening a FIFO with no reader blocks, and monitrs would hang
        // before it could tell anyone why.
        let dir = TempDir::new("socket-sink");
        let path = dir.file("monitrs.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let error = BoundedLogFile::open(&path, DEFAULT_MAX_BYTES).expect_err("refused");
        drop(listener);
        assert!(
            matches!(error, LogError::NotARegularFile { kind: "socket", .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("debug log"),
            "the message must say what it is about: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_character_device_is_refused_because_a_device_is_where_the_display_lives() {
        // `/dev/null` is the harmless member of the class and exists everywhere;
        // `/dev/tty` is the dangerous one and is refused by name as well.
        let error = BoundedLogFile::open(Path::new("/dev/null"), DEFAULT_MAX_BYTES)
            .expect_err("a device is not a log file");
        assert!(
            matches!(
                error,
                LogError::NotARegularFile {
                    kind: "character device",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_named_pipe_is_refused_without_waiting_for_a_reader() {
        // The refusal has to happen *before* the open, because opening a FIFO with no
        // reader blocks indefinitely and monitrs would hang with nothing on screen to
        // explain why. So this test asserts two things at once: that the answer is a
        // refusal, and that it arrives at all. The work is done on a spawned thread so
        // that a regression fails the test instead of hanging CI forever.
        let dir = TempDir::new("fifo-sink");
        let path = dir.file("monitrs.fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(
            made,
            "mkfifo is POSIX and both v1 targets have it; without it this rule is untested"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || {
            let outcome = BoundedLogFile::open(&probe, DEFAULT_MAX_BYTES).map(|_| ());
            let _ = tx.send(outcome.err().map(|error| error.to_string()));
        });
        let answered = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("§14.2: opening a FIFO must not block startup");
        let message = answered.expect("a named pipe cannot hold the debug log");
        assert!(message.contains("named pipe"), "{message}");
    }

    #[test]
    fn an_unopenable_path_is_reported_and_monitrs_still_starts() {
        let dir = TempDir::new("unopenable");
        // A directory is never a valid log file.
        let settings = LogSettings::to_file(dir.file(""));
        let startup = install(&settings);
        assert!(!startup.is_logging());
        assert_eq!(
            startup.problems.len(),
            1,
            "exactly one problem: {:?}",
            startup.problems
        );
        assert!(
            startup
                .problems
                .first()
                .is_some_and(|problem| problem.contains("debug log")),
            "{:?}",
            startup.problems
        );
    }

    #[test]
    fn an_empty_path_is_a_problem_rather_than_a_file_called_nothing() {
        let error = BoundedLogFile::open(Path::new(""), DEFAULT_MAX_BYTES).expect_err("refused");
        assert!(matches!(error, LogError::EmptyPath), "{error}");
    }

    #[test]
    fn a_missing_parent_directory_is_created_rather_than_refused() {
        let dir = TempDir::new("mkparent");
        let path = dir.file("nested/deeper/monitrs.log");
        let sink = BoundedLogFile::open(&path, DEFAULT_MAX_BYTES).expect("opens");
        drop(sink);
        assert!(path.is_file(), "--debug-log should not require mkdir first");
    }

    #[test]
    fn the_default_directives_parse_so_the_fallback_is_never_needed() {
        assert!(
            EnvFilter::try_new(DEFAULT_DIRECTIVES).is_ok(),
            "the constant must be valid, or every run silently loses its filter"
        );
    }

    #[test]
    fn an_explicit_filter_wins_over_the_environment() {
        let (_, problems) = build_filter(Some("monitrs=trace"), Some("monitrs=error"));
        assert!(problems.is_empty());
    }

    #[test]
    fn an_unparsable_filter_falls_back_to_the_default_and_says_so() {
        let (_, problems) = build_filter(Some("=nonsense=="), None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems
                .first()
                .is_some_and(|problem| problem.contains("default log filter")),
            "{problems:?}"
        );

        let (_, from_env) = build_filter(None, Some("=nonsense=="));
        assert!(
            from_env
                .first()
                .is_some_and(|problem| problem.contains(FILTER_ENV)),
            "the message must name the source: {from_env:?}"
        );
    }

    #[test]
    fn a_blank_filter_is_treated_as_absent() {
        let (_, problems) = build_filter(Some("   "), None);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_filter_that_excludes_our_crate_keeps_the_line_out_of_the_file() {
        // Proof the filter is wired in at all: without it, `with_default` would
        // record everything and the assertion below could never fail.
        let dir = TempDir::new("filtered");
        let settings = LogSettings::to_file(dir.file("monitrs.log")).with_directives("monitrs=off");
        let contents = logged_to_file(&settings, || {
            log_collection(Tier::Fast, Duration::from_millis(5), 1);
        });
        assert!(contents.is_empty(), "{contents}");
    }

    #[test]
    fn nothing_is_printed_while_the_alternate_screen_is_active() {
        assert_eq!(destination(true), ProblemDestination::Deferred);
        assert_eq!(destination(false), ProblemDestination::Stderr);
    }

    #[test]
    fn a_deferred_problem_is_handed_back_for_the_ui_to_show() {
        // No terminal is active in a unit test, so the printable path is the one
        // exercised here; the deferred path is pinned by `destination` above.
        let undelivered = report_problems(&["could not open the log".to_owned()]);
        assert!(
            undelivered.is_empty(),
            "with no UI running the message goes to stderr: {undelivered:?}"
        );
    }

    #[test]
    fn a_running_log_reports_its_own_dropped_lines() {
        let dir = TempDir::new("dropped");
        let settings = LogSettings::to_file(dir.file("monitrs.log"));
        let (subscriber, log, _) = build(&settings, None).expect("opens");
        assert_eq!(
            log.dropped_lines(),
            0,
            "nothing dropped before anything ran"
        );
        assert_eq!(log.path(), dir.file("monitrs.log"));
        drop(subscriber);
        log.shutdown();
    }
}
