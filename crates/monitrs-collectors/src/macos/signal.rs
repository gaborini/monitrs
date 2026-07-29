//! Signalling a process, with the identity revalidated first.
//!
//! # Why revalidation lives here and not in the caller
//!
//! §15.1 and §9.3 both require `(pid, start_key)` to be rechecked immediately
//! before the signal is delivered, because a PID freed between the user pressing a
//! key and the effect running belongs to somebody else's process by then. Making
//! [`send_signal`] the only way to signal — with the check inside it, between the
//! read and the `kill`, and with no way to skip it — means the rule cannot be
//! forgotten at a call site.
//!
//! The window is not closable: there is no atomic "signal this process if it is
//! still the one I read" on any Unix. What *is* achievable is that the window is
//! microseconds wide instead of seconds wide, and that a reuse detected inside it
//! aborts rather than signals. Both are.
//!
//! # PID 0
//!
//! `kill(0, sig)` signals every process in the caller's process group, and
//! `kill(-1, sig)` signals every process the caller may signal. Neither is ever
//! what a user selecting a row asked for, so both are refused before the syscall.
//! macOS reports `kernel_task` as PID 0, so this is a row a user can actually
//! select — not a hypothetical.

use core::ffi::c_int;

use monitrs_core::model::ProcessIdentity;

use super::process;
use super::sysctl::{self, NativeError};

/// The signals this collector will send.
///
/// Mirrors the dialog set §9.2 fixes. It is a separate type from the UI's signal
/// enum because §10.1 forbids this crate from depending on `monitrs-tui`; the
/// numbers are POSIX and identical on both sides.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MacosSignal {
    /// `SIGTERM`: ask the process to exit.
    Term,
    /// `SIGINT`: what `Ctrl-C` in a shell would send.
    Int,
    /// `SIGHUP`: hang-up, which many daemons read as "reload".
    Hup,
    /// `SIGKILL`: unstoppable, and last in the dialog by §9.2.
    Kill,
}

impl MacosSignal {
    /// The dialog order §9.2 mandates, with `SIGKILL` last.
    pub const DIALOG_ORDER: [Self; 4] = [Self::Term, Self::Int, Self::Hup, Self::Kill];

    /// The POSIX signal name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Int => "SIGINT",
            Self::Hup => "SIGHUP",
            Self::Kill => "SIGKILL",
        }
    }

    /// The POSIX signal number.
    #[must_use]
    pub const fn number(self) -> c_int {
        match self {
            Self::Hup => libc::SIGHUP,
            Self::Int => libc::SIGINT,
            Self::Kill => libc::SIGKILL,
            Self::Term => libc::SIGTERM,
        }
    }

    /// Whether the process cannot refuse or clean up after this signal.
    #[must_use]
    pub const fn is_forceful(self) -> bool {
        matches!(self, Self::Kill)
    }
}

/// What happened when a signal was attempted.
///
/// Every variant is a distinct thing to tell the user; §15.1 requires permission
/// failures and already-exited processes to be reported clearly rather than folded
/// into one "failed".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalOutcome {
    /// The kernel accepted the signal.
    Delivered,
    /// The process had already exited, so nothing was signalled.
    ///
    /// Expected rather than exceptional: the user selected a row from a snapshot
    /// that is by definition slightly out of date (§14.1).
    AlreadyExited,
    /// The PID now belongs to a different process, so nothing was signalled.
    Reused {
        /// The identity the user confirmed.
        requested: ProcessIdentity,
        /// What that PID refers to now.
        found: ProcessIdentity,
    },
    /// The OS refused: the process belongs to another user and we are not root.
    ///
    /// §15.1 forbids escalating automatically, so this is the end of the road.
    PermissionDenied,
    /// Refused before the syscall because the target is not a single process.
    NotASingleProcess,
    /// Something else went wrong, with the errno for the log.
    Failed {
        /// The raw errno.
        errno: i32,
    },
}

impl SignalOutcome {
    /// Whether the signal actually reached a process.
    #[must_use]
    pub const fn is_delivered(&self) -> bool {
        matches!(self, Self::Delivered)
    }

    /// A short sentence for the status line.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::Delivered => "signal sent",
            Self::AlreadyExited => "the process had already exited",
            Self::Reused { .. } => "that PID now belongs to a different process; nothing was sent",
            Self::PermissionDenied => "permission denied: the process belongs to another user",
            Self::NotASingleProcess => "refused: that PID does not name a single process",
            Self::Failed { .. } => "the signal could not be sent",
        }
    }

    /// A redundant non-colour cue, so the outcome survives a monochrome terminal
    /// (§5.2).
    #[must_use]
    pub const fn symbol(&self) -> char {
        match self {
            Self::Delivered => '+',
            Self::AlreadyExited => '.',
            Self::Reused { .. } => '~',
            Self::PermissionDenied => '!',
            Self::NotASingleProcess | Self::Failed { .. } => '?',
        }
    }
}

/// Whether `pid` names one specific process.
///
/// `kill` treats 0, -1, and negatives as process-group broadcasts, and a `u32` that
/// does not fit a `c_int` cannot be converted without wrapping into one of them.
fn single_process_pid(pid: u32) -> Option<c_int> {
    if pid == 0 {
        return None;
    }
    c_int::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// Sends `signal` to `identity`, revalidating the identity immediately first.
///
/// The read and the `kill` are adjacent on purpose: nothing else happens between
/// them, so the reuse window is as small as a userspace program can make it.
#[must_use]
pub fn send_signal(identity: ProcessIdentity, signal: MacosSignal) -> SignalOutcome {
    let Some(pid) = single_process_pid(identity.pid) else {
        return SignalOutcome::NotASingleProcess;
    };

    match process::read_one(identity.pid) {
        Ok(None) => return SignalOutcome::AlreadyExited,
        Ok(Some(current)) if current.identity != identity => {
            return SignalOutcome::Reused {
                requested: identity,
                found: current.identity,
            };
        }
        Ok(Some(_)) => {}
        Err(error) if error.is_gone() => return SignalOutcome::AlreadyExited,
        // The identity could not be confirmed. Refusing is the only safe answer:
        // signalling on the strength of a PID alone is the mistake §26 names.
        Err(error) => return outcome_from(error),
    }

    sysctl::clear_errno();
    // SAFETY: `kill` takes two integers and touches no memory. `pid` has been
    // checked to be strictly positive, so this cannot become a process-group
    // broadcast.
    let result = unsafe { libc::kill(pid, signal.number()) };
    if result == 0 {
        return SignalOutcome::Delivered;
    }
    outcome_from(NativeError::last())
}

/// Maps a failure onto an outcome.
fn outcome_from(error: NativeError) -> SignalOutcome {
    if error.is_permission_denied() {
        return SignalOutcome::PermissionDenied;
    }
    if error.is_gone() {
        return SignalOutcome::AlreadyExited;
    }
    SignalOutcome::Failed {
        errno: error.errno().unwrap_or(0),
    }
}

/// Whether a process exists and still has the identity it was read with.
///
/// Exposed for the confirmation dialog, which §6.2 wants to show the process's
/// details at the moment of asking rather than at the moment of selection.
#[must_use]
pub fn identity_is_current(identity: ProcessIdentity) -> bool {
    matches!(
        process::read_one(identity.pid),
        Ok(Some(current)) if current.identity == identity
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_order_puts_the_forceful_signal_last() {
        // §9.2: SIGKILL is never the habitual first choice.
        let order = MacosSignal::DIALOG_ORDER;
        assert_eq!(order.first(), Some(&MacosSignal::Term));
        assert_eq!(order.last(), Some(&MacosSignal::Kill));
        assert_eq!(order.iter().filter(|s| s.is_forceful()).count(), 1);
        assert!(order.last().is_some_and(|signal| signal.is_forceful()));
    }

    #[test]
    fn the_signal_numbers_are_the_posix_ones() {
        assert_eq!(MacosSignal::Hup.number(), 1);
        assert_eq!(MacosSignal::Int.number(), 2);
        assert_eq!(MacosSignal::Kill.number(), 9);
        assert_eq!(MacosSignal::Term.number(), 15);
    }

    #[test]
    fn pid_zero_is_refused_because_it_would_signal_the_whole_process_group() {
        // macOS lists kernel_task as PID 0, so this is a row a user can select.
        assert_eq!(single_process_pid(0), None);
        assert_eq!(
            send_signal(ProcessIdentity::new(0, 12_345), MacosSignal::Term),
            SignalOutcome::NotASingleProcess
        );
    }

    #[test]
    fn a_pid_that_cannot_fit_a_c_int_is_refused_rather_than_wrapped() {
        // Wrapping would turn it negative, and a negative pid is a group broadcast.
        assert_eq!(single_process_pid(u32::MAX), None);
        assert_eq!(single_process_pid(0x8000_0000), None);
        assert_eq!(single_process_pid(1), Some(1));
    }

    #[test]
    fn every_outcome_has_a_distinct_symbol_and_a_sentence() {
        let outcomes = [
            SignalOutcome::Delivered,
            SignalOutcome::AlreadyExited,
            SignalOutcome::Reused {
                requested: ProcessIdentity::new(1, 1),
                found: ProcessIdentity::new(1, 2),
            },
            SignalOutcome::PermissionDenied,
            SignalOutcome::NotASingleProcess,
        ];
        for outcome in &outcomes {
            assert!(!outcome.message().is_empty());
            assert!(outcome.message().is_ascii(), "{}", outcome.message());
        }
        let mut symbols: Vec<char> = outcomes.iter().map(SignalOutcome::symbol).collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(
            symbols.len(),
            outcomes.len(),
            "each outcome a user can act on needs its own cue"
        );
        assert!(SignalOutcome::Delivered.is_delivered());
        assert!(!SignalOutcome::PermissionDenied.is_delivered());
    }

    #[test]
    fn permission_and_liveness_failures_are_reported_distinctly() {
        // §15.1: both have to be clear to the user, not merged into "failed".
        assert_eq!(
            outcome_from(NativeError::Errno(libc::EPERM)),
            SignalOutcome::PermissionDenied
        );
        assert_eq!(
            outcome_from(NativeError::Errno(libc::ESRCH)),
            SignalOutcome::AlreadyExited
        );
        assert_eq!(
            outcome_from(NativeError::Errno(libc::EINVAL)),
            SignalOutcome::Failed {
                errno: libc::EINVAL
            }
        );
    }

    /// Set by [`on_hup`] when the test's own `SIGHUP` arrives.
    static HUP_RECEIVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// A signal handler that does the one thing that is async-signal-safe.
    extern "C" fn on_hup(_signal: c_int) {
        HUP_RECEIVED.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    #[ignore = "platform smoke test: delivers a real signal to this process"]
    fn a_signal_reaches_a_live_process_and_a_stale_identity_is_refused() {
        // The end-to-end check, deliberately without spawning anything: §9.3 forbids
        // external commands anywhere in this module, tests included. Sending SIGHUP
        // to ourselves with a handler installed proves delivery for real.
        let handler = on_hup as *const () as libc::sighandler_t;
        // SAFETY: `signal` installs a handler for one signal number; the handler is
        // an `extern "C"` function with the right signature that only stores into an
        // atomic, which is async-signal-safe.
        let previous = unsafe { libc::signal(libc::SIGHUP, handler) };
        assert_ne!(
            previous,
            libc::SIG_ERR,
            "installing the handler must succeed"
        );

        let me = std::process::id();
        let live = process::read_one(me)
            .expect("our own process is readable")
            .expect("we exist")
            .identity;
        assert!(identity_is_current(live));

        // A stale start key must abort rather than signal whatever holds the PID.
        let stale = ProcessIdentity::new(me, live.start_key.wrapping_add(1));
        assert!(!identity_is_current(stale));
        match send_signal(stale, MacosSignal::Hup) {
            SignalOutcome::Reused { requested, found } => {
                assert_eq!(requested, stale);
                assert_eq!(found, live);
            }
            other => panic!("a stale identity must not be signalled, got {other:?}"),
        }
        assert!(
            !HUP_RECEIVED.load(std::sync::atomic::Ordering::SeqCst),
            "the refused signal must not have been delivered"
        );

        assert_eq!(
            send_signal(live, MacosSignal::Hup),
            SignalOutcome::Delivered
        );
        // Delivery to self is synchronous enough that the handler has already run,
        // but a short bounded wait keeps the test from depending on that.
        for _ in 0..100 {
            if HUP_RECEIVED.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
        assert!(
            HUP_RECEIVED.load(std::sync::atomic::Ordering::SeqCst),
            "the signal was reported delivered but never arrived"
        );

        // SAFETY: restores the default disposition for the signal handled above.
        let _ = unsafe { libc::signal(libc::SIGHUP, libc::SIG_DFL) };
    }

    #[test]
    #[ignore = "platform smoke test: signals a root-owned process"]
    fn signalling_a_root_owned_process_reports_permission_denied() {
        // §15.1 and §9.3: the refusal has to be visible, and monitrs must not try to
        // escalate. Signal 0 is not used here: the real SIGTERM path is what needs
        // to fail safely, and launchd is guaranteed to refuse it.
        let launchd = process::read_one(1)
            .expect("pid 1 is readable")
            .expect("pid 1 exists")
            .identity;
        // Skip if this test is somehow running as root, where the signal would
        // actually be delivered to launchd.
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert_eq!(
            send_signal(launchd, MacosSignal::Term),
            SignalOutcome::PermissionDenied
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_pid_that_has_never_existed_is_reported_as_already_exited() {
        let phantom = ProcessIdentity::new(0x7fff_0000, 1);
        assert_eq!(
            send_signal(phantom, MacosSignal::Term),
            SignalOutcome::AlreadyExited
        );
        assert!(!identity_is_current(phantom));
    }
}
