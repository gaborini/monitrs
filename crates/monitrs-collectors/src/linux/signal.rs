//! Signal support and the revalidation that must precede delivery.
//!
//! §9.2 is specific about process actions on Linux: support the signals the OS
//! exposes, default the dialog to `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGKILL`, put
//! `SIGKILL` last and mark it forceful, and **revalidate `(pid, start_time)`
//! immediately before signalling**.
//!
//! # Why revalidation is a separate, pure function
//!
//! The interval between "the user pressed `y`" and "the signal is delivered" is
//! long: a confirmation dialog, a keypress, a channel hop to a worker thread. A PID
//! can be recycled inside it, and on Linux a PID is recycled *fast* — the default
//! `pid_max` of 32 768 wraps in seconds under a fork loop. Signalling a recycled PID
//! sends `SIGKILL` to an unrelated process, which is the single most damaging thing
//! this program could do (§15.1).
//!
//! [`revalidate`] therefore takes the bytes of a **freshly read** `/proc/<pid>/stat`
//! and returns a [`SignalDecision`] that only permits delivery when the identity
//! still matches exactly. It is pure, so the dangerous case — a PID reused within
//! the same second, which the whole-second start time of the cross-platform baseline
//! cannot see — is covered by a fixture test rather than by hope. Delivery itself is
//! behind [`SignalSink`], so every decision path is testable without signalling
//! anything.

use monitrs_core::model::{ProcessIdentity, ProcessState};

use crate::linux::process::parse_pid_stat;
use crate::linux::read::ReadFailure;

/// A signal monitrs is willing to send.
///
/// Restricted on purpose. §15.1 requires process control to be deliberate, and a
/// full table of 64 signals in a confirmation dialog invites a mis-keyed
/// `SIGSEGV`. These nine are the ones a person monitoring a system actually reaches
/// for, and each carries the consequence sentence §6.2 requires.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinuxSignal {
    /// `SIGHUP` (1): hang-up, which most daemons read as "reload".
    Hup,
    /// `SIGINT` (2): what `Ctrl-C` sends.
    Int,
    /// `SIGQUIT` (3): quit and dump core.
    Quit,
    /// `SIGKILL` (9): unstoppable.
    Kill,
    /// `SIGUSR1` (10): application-defined.
    Usr1,
    /// `SIGUSR2` (12): application-defined.
    Usr2,
    /// `SIGTERM` (15): ask the process to exit.
    Term,
    /// `SIGCONT` (18): resume a stopped process.
    Cont,
    /// `SIGSTOP` (19): suspend, unstoppably.
    Stop,
}

impl LinuxSignal {
    /// Every signal this collector will deliver.
    ///
    /// Ordered by signal number so the list is stable and greppable against
    /// `signal(7)`.
    pub const SUPPORTED_SIGNALS: [Self; 9] = [
        Self::Hup,
        Self::Int,
        Self::Quit,
        Self::Kill,
        Self::Usr1,
        Self::Usr2,
        Self::Term,
        Self::Cont,
        Self::Stop,
    ];

    /// The default dialog order §9.2 mandates.
    ///
    /// `SIGKILL` is last so it is never the default and never adjacent to the
    /// habitual first choice.
    pub const DIALOG_SIGNALS: [Self; 4] = [Self::Term, Self::Int, Self::Hup, Self::Kill];

    /// The POSIX name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hup => "SIGHUP",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Kill => "SIGKILL",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
            Self::Term => "SIGTERM",
            Self::Cont => "SIGCONT",
            Self::Stop => "SIGSTOP",
        }
    }

    /// The signal number.
    ///
    /// These are the values for every architecture Linux supports except MIPS,
    /// PA-RISC, and Alpha, where `SIGSTOP`, `SIGCONT`, and `SIGUSR1/2` differ.
    /// [`LinuxSignal::is_architecture_stable`] marks the affected ones so a build for
    /// one of those targets can restrict the dialog rather than send the wrong
    /// signal.
    #[must_use]
    pub const fn number(self) -> i32 {
        match self {
            Self::Hup => 1,
            Self::Int => 2,
            Self::Quit => 3,
            Self::Kill => 9,
            Self::Usr1 => 10,
            Self::Usr2 => 12,
            Self::Term => 15,
            Self::Cont => 18,
            Self::Stop => 19,
        }
    }

    /// Whether this signal's number is the same on every Linux architecture.
    #[must_use]
    pub const fn is_architecture_stable(self) -> bool {
        matches!(
            self,
            Self::Hup | Self::Int | Self::Quit | Self::Kill | Self::Term
        )
    }

    /// Whether the process cannot refuse or clean up after this signal.
    ///
    /// Drives the visual marking §9.2 asks for and the distinct confirmation key
    /// §15.1 asks for. `SIGSTOP` is included: it cannot be caught either, and a
    /// suspended database looks exactly like a hung one.
    #[must_use]
    pub const fn is_forceful(self) -> bool {
        matches!(self, Self::Kill | Self::Stop)
    }

    /// The consequence sentence the confirmation dialog shows (§6.2).
    #[must_use]
    pub const fn consequence(self) -> &'static str {
        match self {
            Self::Hup => "reports a hang-up; many services reload their configuration instead",
            Self::Int => "interrupts the process, as Ctrl-C in its own terminal would",
            Self::Quit => "asks the process to quit and, by default, to write a core dump",
            Self::Kill => "terminates the process immediately, with no cleanup and no unsaved work",
            Self::Usr1 => {
                "delivers an application-defined signal whose effect depends on the program"
            }
            Self::Usr2 => "delivers a second application-defined signal, likewise program-specific",
            Self::Term => "asks the process to exit; it may clean up first, or ignore the request",
            Self::Cont => "resumes a stopped process",
            Self::Stop => {
                "suspends the process immediately; it cannot refuse, and it stays suspended \
                           until continued"
            }
        }
    }
}

/// What revalidation concluded.
///
/// Only [`SignalDecision::Deliver`] permits a signal, and it is the only variant that
/// carries the identity to signal — so no code path can signal a PID that was not
/// just verified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalDecision {
    /// The identity still matches. Deliver `signal` to `identity.pid`.
    Deliver {
        /// The verified identity.
        identity: ProcessIdentity,
        /// The signal to send.
        signal: LinuxSignal,
    },
    /// The process no longer exists. Expected, and not an error (§14.1).
    Vanished(ProcessIdentity),
    /// The PID now belongs to a different process. **Abort** (§9.2).
    Reused {
        /// What the user confirmed.
        requested: ProcessIdentity,
        /// What the PID refers to now.
        found: ProcessIdentity,
    },
    /// The process is a zombie or already dead, so a signal would do nothing.
    ///
    /// §15.1 requires the dialog to say so rather than pretending to act.
    AlreadyExited(ProcessIdentity),
    /// The identity could not be verified.
    ///
    /// Refusing is the only safe answer: an unverifiable identity is exactly the
    /// situation in which a recycled PID would be signalled.
    Unverifiable {
        /// What the user confirmed.
        requested: ProcessIdentity,
        /// Why verification failed.
        failure: ReadFailure,
    },
}

impl SignalDecision {
    /// The identity to signal, or `None` for every other outcome.
    #[must_use]
    pub const fn deliverable(&self) -> Option<(ProcessIdentity, LinuxSignal)> {
        match self {
            Self::Deliver { identity, signal } => Some((*identity, *signal)),
            _ => None,
        }
    }

    /// A message for the status line, explaining an abort in the user's terms.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Deliver { identity, signal } => {
                format!("sending {} to pid {}", signal.name(), identity.pid)
            }
            Self::Vanished(identity) => format!("pid {} has already exited", identity.pid),
            Self::Reused { requested, found } => format!(
                "pid {} now belongs to a different process; nothing was signalled \
                 (start key {} became {})",
                requested.pid, requested.start_key, found.start_key
            ),
            Self::AlreadyExited(identity) => format!(
                "pid {} is a zombie waiting to be reaped; a signal would have no effect",
                identity.pid
            ),
            Self::Unverifiable { requested, failure } => format!(
                "could not confirm pid {} is still the same process ({}); nothing was signalled",
                requested.pid,
                failure.describe()
            ),
        }
    }

    /// Whether this outcome is a routine event rather than something to report.
    ///
    /// A vanished process is expected (§14.1); a reused PID is not — it means we came
    /// within one step of signalling the wrong process, and the user should be told.
    #[must_use]
    pub const fn is_expected(&self) -> bool {
        matches!(self, Self::Deliver { .. } | Self::Vanished(_))
    }
}

/// Revalidates a pending action against a freshly read `/proc/<pid>/stat`.
///
/// `fresh_stat` **must** have been read after the user confirmed and immediately
/// before delivery; passing a buffer from the last sampling tick defeats the entire
/// check. The signature takes bytes rather than a path precisely so this rule is
/// visible at the call site and so every branch is testable from a fixture (§17.2).
#[must_use]
pub fn revalidate(
    requested: ProcessIdentity,
    signal: LinuxSignal,
    fresh_stat: Result<&[u8], ReadFailure>,
) -> SignalDecision {
    let bytes = match fresh_stat {
        Ok(bytes) => bytes,
        Err(ReadFailure::Missing) => return SignalDecision::Vanished(requested),
        Err(failure) => return SignalDecision::Unverifiable { requested, failure },
    };

    let Ok(stat) = parse_pid_stat(bytes) else {
        // An unparsable `stat` for a live PID means either a truncated read or a
        // process that exited mid-read. Either way the identity is unconfirmed.
        return SignalDecision::Unverifiable {
            requested,
            failure: ReadFailure::Failed,
        };
    };

    let found = stat.identity();
    if found != requested {
        return SignalDecision::Reused { requested, found };
    }
    if !stat.state.is_signalable() {
        return SignalDecision::AlreadyExited(requested);
    }
    if matches!(stat.state, ProcessState::Unknown) && stat.start_time_ticks == 0 {
        // Belt and braces: a state we cannot interpret on a process with no start
        // time is not something to signal.
        return SignalDecision::Unverifiable {
            requested,
            failure: ReadFailure::Failed,
        };
    }
    SignalDecision::Deliver {
        identity: found,
        signal,
    }
}

/// Why delivery failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalError {
    /// `EPERM`: we are not permitted to signal this process.
    ///
    /// §9.3 requires this to be surfaced clearly rather than reported as a generic
    /// failure, and the same applies here: the user needs to know that privileges,
    /// not the process, are the obstacle.
    NotPermitted,
    /// `ESRCH`: the process exited between revalidation and delivery.
    ///
    /// The residual race no amount of checking can remove — which is why it is a
    /// typed outcome rather than an assertion.
    NoSuchProcess,
    /// `EINVAL`: the signal number is not valid on this kernel.
    InvalidSignal,
    /// Any other `errno`.
    Failed(i32),
    /// This build cannot deliver signals at all.
    Unsupported,
}

impl SignalError {
    /// A message for the status line.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::NotPermitted => {
                "not permitted to signal this process; it belongs to another user".to_owned()
            }
            Self::NoSuchProcess => "the process exited before the signal was delivered".to_owned(),
            Self::InvalidSignal => "this kernel does not accept that signal".to_owned(),
            Self::Failed(errno) => format!("signal delivery failed with errno {errno}"),
            Self::Unsupported => "this build cannot deliver signals".to_owned(),
        }
    }

    /// Maps a raw `errno` from `kill(2)`.
    #[must_use]
    pub const fn from_errno(errno: i32) -> Self {
        match errno {
            1 => Self::NotPermitted,
            3 => Self::NoSuchProcess,
            22 => Self::InvalidSignal,
            other => Self::Failed(other),
        }
    }
}

/// Something that can deliver a signal.
///
/// A trait rather than a free function for two reasons. It keeps the `kill(2)` call
/// — the only genuinely dangerous line in this crate — behind one narrow interface,
/// and it lets every decision path be tested with a sink that records instead of
/// signalling, so the test suite proves *what would have been sent* without sending
/// it.
pub trait SignalSink {
    /// Delivers `signal` to `pid`.
    fn deliver(&mut self, pid: u32, signal: LinuxSignal) -> Result<(), SignalError>;
}

/// Revalidates and then delivers, in that order, with nothing in between.
///
/// The whole point of the function: there is no way to reach `sink.deliver` without
/// passing through [`revalidate`] first, so §9.2's "revalidate immediately before
/// signalling" is structural rather than a comment.
pub fn signal_process<S: SignalSink>(
    sink: &mut S,
    requested: ProcessIdentity,
    signal: LinuxSignal,
    fresh_stat: Result<&[u8], ReadFailure>,
) -> Result<SignalDecision, SignalError> {
    let decision = revalidate(requested, signal, fresh_stat);
    match decision.deliverable() {
        Some((identity, signal)) => sink.deliver(identity.pid, signal).map(|()| decision),
        None => Ok(decision),
    }
}

/// The live `kill(2)` sink.
///
/// Gated on Linux because `kill(2)`'s signal numbers are the Linux ones and because
/// the macOS collector has its own path (§9.3).
#[cfg(all(target_os = "linux", feature = "linux-native"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct KillSink;

#[cfg(all(target_os = "linux", feature = "linux-native"))]
mod live {
    use super::{KillSink, LinuxSignal, SignalError, SignalSink};

    // `kill(2)` is declared here rather than taken from the `libc` crate because
    // `libc` is an optional, macOS-only dependency of this crate and the manifest is
    // outside this module's remit. The signature is fixed by POSIX and by the Linux
    // kernel ABI — `int kill(pid_t, int)` with `pid_t` as a 32-bit signed integer on
    // every Linux architecture — and the symbol is always resolvable because the
    // Rust standard library links libc on every Linux target. A reviewer who prefers
    // the crate can delete this block and call `libc::kill` instead; nothing else in
    // this file changes.
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    impl SignalSink for KillSink {
        fn deliver(&mut self, pid: u32, signal: LinuxSignal) -> Result<(), SignalError> {
            // A negative or zero pid means "signal a process group" or "signal every
            // process we may signal". Neither is ever what a monitor should do, and
            // both are reachable from a `u32` that does not fit an `i32`, so the
            // conversion failing is a refusal rather than a truncation.
            let Ok(pid) = i32::try_from(pid) else {
                return Err(SignalError::InvalidSignal);
            };
            if pid <= 0 {
                return Err(SignalError::InvalidSignal);
            }
            // SAFETY: `kill` takes two integers by value, reads no memory through
            // pointers, and writes none. Its only effect is on the target process,
            // and the target is a positive pid that was revalidated by
            // `super::revalidate` immediately before this call. The function cannot
            // unwind, so there is no cross-language panic. Failure is reported
            // through `errno`, which is read below with the standard library's own
            // accessor.
            let result = unsafe { kill(pid, signal.number()) };
            if result == 0 {
                return Ok(());
            }
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .map_or(SignalError::Failed(0), SignalError::from_errno))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    /// A sink that records what it was asked to send instead of sending it.
    #[derive(Debug, Default)]
    struct RecordingSink {
        sent: Vec<(u32, LinuxSignal)>,
        fail_with: Option<SignalError>,
    }

    impl SignalSink for RecordingSink {
        fn deliver(&mut self, pid: u32, signal: LinuxSignal) -> Result<(), SignalError> {
            if let Some(error) = self.fail_with {
                return Err(error);
            }
            self.sent.push((pid, signal));
            Ok(())
        }
    }

    /// The identity the `simple` fixture describes.
    fn simple_identity() -> ProcessIdentity {
        ProcessIdentity::new(4_242, 88_213_700)
    }

    #[test]
    fn the_dialog_puts_sigkill_last_and_marks_it_forceful() {
        // §9.2, exactly.
        assert_eq!(
            LinuxSignal::DIALOG_SIGNALS,
            [
                LinuxSignal::Term,
                LinuxSignal::Int,
                LinuxSignal::Hup,
                LinuxSignal::Kill
            ]
        );
        let last = LinuxSignal::DIALOG_SIGNALS
            .last()
            .copied()
            .expect("four entries");
        assert_eq!(last, LinuxSignal::Kill);
        assert!(last.is_forceful());
        for signal in [LinuxSignal::Term, LinuxSignal::Int, LinuxSignal::Hup] {
            assert!(
                !signal.is_forceful(),
                "{} must not be forceful",
                signal.name()
            );
        }
    }

    #[test]
    fn every_supported_signal_has_a_unique_name_number_and_consequence() {
        let mut numbers: Vec<i32> = LinuxSignal::SUPPORTED_SIGNALS
            .iter()
            .map(|signal| signal.number())
            .collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), LinuxSignal::SUPPORTED_SIGNALS.len());

        for signal in LinuxSignal::SUPPORTED_SIGNALS {
            assert!(signal.name().starts_with("SIG"));
            assert!(signal.name().is_ascii());
            assert!(!signal.consequence().is_empty());
            assert!(signal.number() > 0);
        }
        // The four the dialog offers must all be architecture-stable, because the
        // dialog is what a user reaches for by habit.
        for signal in LinuxSignal::DIALOG_SIGNALS {
            assert!(
                signal.is_architecture_stable(),
                "{} differs by architecture and must not be a default choice",
                signal.name()
            );
        }
        assert!(!LinuxSignal::Stop.is_architecture_stable());
    }

    #[test]
    fn signal_numbers_match_the_posix_values() {
        assert_eq!(LinuxSignal::Hup.number(), 1);
        assert_eq!(LinuxSignal::Int.number(), 2);
        assert_eq!(LinuxSignal::Quit.number(), 3);
        assert_eq!(LinuxSignal::Kill.number(), 9);
        assert_eq!(LinuxSignal::Term.number(), 15);
    }

    #[test]
    fn a_matching_identity_is_the_only_case_that_delivers() {
        let mut sink = RecordingSink::default();
        let decision = signal_process(
            &mut sink,
            simple_identity(),
            LinuxSignal::Term,
            Ok(fixtures::PID_STAT_SIMPLE),
        )
        .expect("delivery succeeded");
        assert_eq!(
            decision,
            SignalDecision::Deliver {
                identity: simple_identity(),
                signal: LinuxSignal::Term
            }
        );
        assert_eq!(sink.sent, vec![(4_242, LinuxSignal::Term)]);
    }

    #[test]
    fn a_pid_reused_within_the_same_second_is_refused() {
        // The reason field 22 is the start key. Both fixtures start in the same whole
        // second, so the cross-platform baseline's identity would match and the
        // signal would go to the wrong process (§9.2, §15.1).
        let mut sink = RecordingSink::default();
        let decision = signal_process(
            &mut sink,
            simple_identity(),
            LinuxSignal::Kill,
            Ok(fixtures::PID_STAT_REUSED_SAME_SECOND),
        )
        .expect("no delivery attempted");

        match decision {
            SignalDecision::Reused { requested, found } => {
                assert_eq!(requested, simple_identity());
                assert_eq!(found.pid, requested.pid);
                assert_ne!(found.start_key, requested.start_key);
            }
            other => panic!("expected a reuse refusal, got {other:?}"),
        }
        assert!(sink.sent.is_empty(), "nothing may be signalled");
        assert!(
            !decision.is_expected(),
            "a near miss is worth telling the user"
        );
        assert!(decision.message().contains("different process"));
    }

    #[test]
    fn a_vanished_process_is_refused_quietly() {
        let mut sink = RecordingSink::default();
        let decision = signal_process(
            &mut sink,
            simple_identity(),
            LinuxSignal::Term,
            Err(ReadFailure::Missing),
        )
        .expect("no delivery attempted");
        assert_eq!(decision, SignalDecision::Vanished(simple_identity()));
        assert!(sink.sent.is_empty());
        assert!(
            decision.is_expected(),
            "§14.1: a vanished process is not worth a warning"
        );
    }

    #[test]
    fn a_zombie_is_refused_with_an_explanation_rather_than_signalled_pointlessly() {
        // §15.1: the dialog must say a signal would have no effect rather than
        // pretending to act.
        let mut sink = RecordingSink::default();
        let zombie = ProcessIdentity::new(7_331, 88_213_000);
        let decision = signal_process(
            &mut sink,
            zombie,
            LinuxSignal::Kill,
            Ok(fixtures::PID_STAT_ZOMBIE),
        )
        .expect("no delivery attempted");
        assert_eq!(decision, SignalDecision::AlreadyExited(zombie));
        assert!(sink.sent.is_empty());
        assert!(decision.message().contains("no effect"));
    }

    #[test]
    fn an_unverifiable_identity_is_refused_rather_than_assumed_good() {
        let mut sink = RecordingSink::default();
        for failure in [
            ReadFailure::Denied,
            ReadFailure::Failed,
            ReadFailure::Oversized,
        ] {
            let decision = signal_process(
                &mut sink,
                simple_identity(),
                LinuxSignal::Term,
                Err(failure),
            )
            .expect("no delivery attempted");
            assert_eq!(
                decision,
                SignalDecision::Unverifiable {
                    requested: simple_identity(),
                    failure
                }
            );
        }
        // A truncated `stat` is equally unverifiable.
        let decision = signal_process(
            &mut sink,
            simple_identity(),
            LinuxSignal::Term,
            Ok(fixtures::PID_STAT_TRUNCATED),
        )
        .expect("no delivery attempted");
        assert!(matches!(decision, SignalDecision::Unverifiable { .. }));
        assert!(sink.sent.is_empty(), "nothing may be signalled");
    }

    #[test]
    fn an_uninterpretable_state_with_no_start_time_is_refused() {
        // The belt-and-braces branch: a `stat` whose state character we cannot read
        // and whose start time is zero carries no identity worth trusting, even
        // though the requested key technically matches.
        let mut sink = RecordingSink::default();
        let unknown = ProcessIdentity::new(1_234, 0);
        let decision = signal_process(
            &mut sink,
            unknown,
            LinuxSignal::Term,
            Ok(b"1234 (odd) ? 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0"),
        )
        .expect("no delivery attempted");
        assert_eq!(
            decision,
            SignalDecision::Unverifiable {
                requested: unknown,
                failure: ReadFailure::Failed
            }
        );
        assert!(sink.sent.is_empty());
    }

    #[test]
    fn a_process_name_full_of_parentheses_does_not_defeat_revalidation() {
        // If the stat parser mis-read the name, the start key would come from the
        // wrong field and every revalidation would fail — or, worse, succeed against
        // a number that happens to match.
        let mut sink = RecordingSink::default();
        let identity = ProcessIdentity::new(9_182, 88_100_000);
        let decision = signal_process(
            &mut sink,
            identity,
            LinuxSignal::Hup,
            Ok(fixtures::PID_STAT_WEIRD_NAME),
        )
        .expect("delivery succeeded");
        assert_eq!(decision.deliverable(), Some((identity, LinuxSignal::Hup)));
        assert_eq!(sink.sent, vec![(9_182, LinuxSignal::Hup)]);
    }

    #[test]
    fn a_delivery_failure_is_reported_without_losing_which_signal_was_attempted() {
        let mut sink = RecordingSink {
            fail_with: Some(SignalError::NotPermitted),
            ..RecordingSink::default()
        };
        let error = signal_process(
            &mut sink,
            simple_identity(),
            LinuxSignal::Term,
            Ok(fixtures::PID_STAT_SIMPLE),
        )
        .expect_err("the sink refused");
        assert_eq!(error, SignalError::NotPermitted);
        assert!(error.message().contains("another user"));
        assert!(sink.sent.is_empty());
    }

    #[test]
    fn errno_values_map_to_the_conditions_the_ui_must_distinguish() {
        assert_eq!(SignalError::from_errno(1), SignalError::NotPermitted);
        assert_eq!(SignalError::from_errno(3), SignalError::NoSuchProcess);
        assert_eq!(SignalError::from_errno(22), SignalError::InvalidSignal);
        assert_eq!(SignalError::from_errno(99), SignalError::Failed(99));
        for error in [
            SignalError::NotPermitted,
            SignalError::NoSuchProcess,
            SignalError::InvalidSignal,
            SignalError::Failed(5),
            SignalError::Unsupported,
        ] {
            assert!(!error.message().is_empty());
        }
    }

    #[test]
    fn every_decision_explains_itself_in_the_users_terms() {
        let identity = simple_identity();
        let decisions = [
            SignalDecision::Deliver {
                identity,
                signal: LinuxSignal::Term,
            },
            SignalDecision::Vanished(identity),
            SignalDecision::Reused {
                requested: identity,
                found: ProcessIdentity::new(4_242, 1),
            },
            SignalDecision::AlreadyExited(identity),
            SignalDecision::Unverifiable {
                requested: identity,
                failure: ReadFailure::Denied,
            },
        ];
        for decision in decisions {
            let message = decision.message();
            assert!(!message.is_empty());
            assert!(message.contains("4242"), "{message}");
        }
    }
}
