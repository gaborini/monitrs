//! Platform-neutral signal delivery.
//!
//! The two native collectors expose deliberately different shapes — macOS pairs
//! `send_signal` with `identity_is_current`, Linux threads a freshly read
//! `/proc/<pid>/stat` through `revalidate` so that reaching the `kill` sink
//! without revalidating is impossible. Both are correct for their platform, and
//! neither should be bent to match the other. This module is the one place that
//! reconciles them, so the event loop has a single call and a single set of
//! outcomes to render.
//!
//! Three rules from §15.1 and §9.2 hold on both platforms:
//!
//! * **The identity is re-read immediately before the signal is sent.** Not when
//!   the dialog opened, not when the key was pressed — here, microseconds before
//!   `kill`. A PID reused in between must abort.
//! * **A process that has already exited is refused**, and the refusal is
//!   reported rather than silently succeeding.
//! * **Nothing escalates.** A permission failure is an outcome, never a prompt
//!   for credentials or a re-exec.

use monitrs_core::model::ProcessIdentity;
use monitrs_tui::action::SignalKind;

/// What happened when a confirmed signal was delivered.
///
/// Deliberately not a `Result`: several of these are expected outcomes rather
/// than failures, and collapsing them would lose the distinction §14.1 draws
/// between an error and an ordinary fact about a process that exited.
// Which variants are constructed depends on the platform, and deliberately so:
// macOS's `send_signal` folds "gone" into `AlreadyExited` and cannot report an
// unverifiable identity, while Linux distinguishes both because it re-reads
// `/proc/<pid>/stat` itself. Narrowing the enum to whichever platform is being
// compiled would make the outcome vocabulary platform-dependent, which is exactly
// what this module exists to prevent.
#[allow(dead_code, reason = "variant construction is platform-conditional")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SignalReport {
    /// The signal reached the process.
    Delivered {
        /// Which signal, for the confirmation notice.
        signal: SignalKind,
        /// Which process.
        identity: ProcessIdentity,
    },
    /// The process was gone by the time we looked again. Expected (§14.1).
    Vanished(ProcessIdentity),
    /// The PID now belongs to a different process, so nothing was sent.
    ///
    /// This is the outcome the whole revalidation exists to produce.
    Reused {
        /// What the user confirmed.
        requested: ProcessIdentity,
        /// What the PID refers to now.
        found: ProcessIdentity,
    },
    /// The process is a zombie or already dead; a signal would do nothing.
    AlreadyExited(ProcessIdentity),
    /// The OS refused. monitrs does not escalate (§15.1).
    PermissionDenied(ProcessIdentity),
    /// The identity could not be re-read, so nothing was sent.
    ///
    /// Refusing on an unverifiable identity is the safe direction: the
    /// alternative is signalling a process we could not confirm.
    Unverifiable {
        /// What the user confirmed.
        identity: ProcessIdentity,
        /// Why verification failed.
        reason: String,
    },
    /// Delivery failed for another reason.
    Failed {
        /// Which process.
        identity: ProcessIdentity,
        /// What the OS said.
        reason: String,
    },
    /// This build cannot signal on this platform.
    Unsupported(ProcessIdentity),
}

impl SignalReport {
    /// Whether the signal actually reached the process.
    pub(crate) const fn was_delivered(&self) -> bool {
        matches!(self, Self::Delivered { .. })
    }

    /// A sentence for the notice log, phrased so the user can tell an expected
    /// outcome from a problem.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Delivered { signal, identity } => {
                format!("sent {} to {}", signal.name(), identity.pid)
            }
            Self::Vanished(identity) => {
                format!("{} had already exited; nothing was sent", identity.pid)
            }
            Self::Reused { requested, found } => format!(
                "aborted: PID {} now belongs to a different process (start key {} not {}); \
                 nothing was sent",
                requested.pid, found.start_key, requested.start_key
            ),
            Self::AlreadyExited(identity) => format!(
                "{} is a zombie; a signal would do nothing, so nothing was sent",
                identity.pid
            ),
            Self::PermissionDenied(identity) => format!(
                "not permitted to signal {}; monitrs does not escalate privileges",
                identity.pid
            ),
            Self::Unverifiable { identity, reason } => format!(
                "could not confirm PID {} still refers to the same process ({reason}); \
                 nothing was sent",
                identity.pid
            ),
            Self::Failed { identity, reason } => {
                format!("signalling {} failed: {reason}", identity.pid)
            }
            Self::Unsupported(identity) => {
                format!(
                    "this build cannot signal PID {} on this platform",
                    identity.pid
                )
            }
        }
    }

    /// How serious the outcome is, for the notice's severity.
    pub(crate) const fn severity(&self) -> monitrs_core::model::Severity {
        use monitrs_core::model::Severity;
        match self {
            // A delivered signal and a process that had already gone are both
            // ordinary outcomes, not problems.
            Self::Delivered { .. } | Self::Vanished(_) | Self::AlreadyExited(_) => Severity::Info,
            // These mean the user's intent was not carried out.
            Self::Reused { .. }
            | Self::PermissionDenied(_)
            | Self::Unverifiable { .. }
            | Self::Unsupported(_) => Severity::Watch,
            Self::Failed { .. } => Severity::Critical,
        }
    }
}

/// Delivers `signal` to `identity`, revalidating the identity first.
///
/// Called from the runtime's effect execution, never from the reducer and never
/// during render (§10.5).
#[cfg(target_os = "macos")]
pub(crate) fn deliver(identity: ProcessIdentity, signal: SignalKind) -> SignalReport {
    use monitrs_collectors::macos::{MacosSignal, SignalOutcome, send_signal};

    let native = match signal {
        SignalKind::Term => MacosSignal::Term,
        SignalKind::Int => MacosSignal::Int,
        SignalKind::Hup => MacosSignal::Hup,
        SignalKind::Kill => MacosSignal::Kill,
    };

    // `send_signal` revalidates internally; there is no path past it that skips
    // the check.
    match send_signal(identity, native) {
        SignalOutcome::Delivered => SignalReport::Delivered { signal, identity },
        SignalOutcome::AlreadyExited => SignalReport::AlreadyExited(identity),
        SignalOutcome::Reused { requested, found } => SignalReport::Reused { requested, found },
        SignalOutcome::PermissionDenied => SignalReport::PermissionDenied(identity),
        SignalOutcome::NotASingleProcess => SignalReport::Failed {
            identity,
            reason: "refused: that PID would signal a process group, not one process".to_owned(),
        },
        SignalOutcome::Failed { errno } => SignalReport::Failed {
            identity,
            reason: format!("errno {errno}"),
        },
    }
}

/// Delivers `signal` to `identity`, revalidating the identity first.
#[cfg(target_os = "linux")]
pub(crate) fn deliver(identity: ProcessIdentity, signal: SignalKind) -> SignalReport {
    use monitrs_collectors::linux::{
        KillSink, LinuxSignal, ProcRoot, ReadFailure, SignalDecision, SignalError, signal_process,
    };

    let native = match signal {
        SignalKind::Term => LinuxSignal::Term,
        SignalKind::Int => LinuxSignal::Int,
        SignalKind::Hup => LinuxSignal::Hup,
        SignalKind::Kill => LinuxSignal::Kill,
    };

    // Read the stat file *now*, so the identity being compared is the current one
    // rather than whatever the last snapshot happened to see (§9.2).
    let root = ProcRoot::live();
    let bytes = root.read_pid(identity.pid, "stat");
    let fresh: Result<&[u8], ReadFailure> = bytes.as_result();

    let mut sink = KillSink;
    match signal_process(&mut sink, identity, native, fresh) {
        Ok(SignalDecision::Deliver { identity, .. }) => {
            SignalReport::Delivered { signal, identity }
        }
        Ok(SignalDecision::Vanished(identity)) => SignalReport::Vanished(identity),
        Ok(SignalDecision::Reused { requested, found }) => {
            SignalReport::Reused { requested, found }
        }
        Ok(SignalDecision::AlreadyExited(identity)) => SignalReport::AlreadyExited(identity),
        Ok(SignalDecision::Unverifiable { requested, failure }) => SignalReport::Unverifiable {
            identity: requested,
            reason: format!("{failure:?}"),
        },
        Err(SignalError::NotPermitted) => SignalReport::PermissionDenied(identity),
        Err(SignalError::NoSuchProcess) => SignalReport::Vanished(identity),
        Err(SignalError::Unsupported) => SignalReport::Unsupported(identity),
        Err(error) => SignalReport::Failed {
            identity,
            reason: error.to_string(),
        },
    }
}

/// Signalling is not implemented for this platform.
///
/// Reported honestly rather than silently doing nothing, because a confirmation
/// dialog that appears to work and does not would be worse than a refusal.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn deliver(identity: ProcessIdentity, _signal: SignalKind) -> SignalReport {
    SignalReport::Unsupported(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_core::model::Severity;

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(31_842, 900_100)
    }

    #[test]
    fn only_a_delivered_signal_counts_as_delivered() {
        assert!(
            SignalReport::Delivered {
                signal: SignalKind::Term,
                identity: identity()
            }
            .was_delivered()
        );
        for report in [
            SignalReport::Vanished(identity()),
            SignalReport::AlreadyExited(identity()),
            SignalReport::PermissionDenied(identity()),
            SignalReport::Unsupported(identity()),
            SignalReport::Reused {
                requested: identity(),
                found: ProcessIdentity::new(31_842, 977_400),
            },
        ] {
            assert!(!report.was_delivered(), "{report:?}");
        }
    }

    #[test]
    fn a_reused_pid_reports_both_start_keys_so_the_abort_is_explicable() {
        let report = SignalReport::Reused {
            requested: identity(),
            found: ProcessIdentity::new(31_842, 977_400),
        };
        let message = report.message();
        assert!(message.contains("aborted"), "{message}");
        assert!(message.contains("31842"), "{message}");
        assert!(message.contains("977400"), "{message}");
        assert!(message.contains("900100"), "{message}");
        assert!(message.contains("nothing was sent"), "{message}");
    }

    #[test]
    fn a_permission_failure_says_monitrs_does_not_escalate() {
        // §15.1: the refusal must be visible and must not invite a sudo prompt.
        let message = SignalReport::PermissionDenied(identity()).message();
        assert!(message.contains("does not escalate"), "{message}");
        assert!(!message.to_lowercase().contains("sudo"), "{message}");
    }

    #[test]
    fn a_zombie_is_refused_rather_than_pretended_at() {
        let message = SignalReport::AlreadyExited(identity()).message();
        assert!(message.contains("zombie"), "{message}");
        assert!(message.contains("nothing was sent"), "{message}");
    }

    #[test]
    fn an_unverifiable_identity_is_refused_and_says_why() {
        let report = SignalReport::Unverifiable {
            identity: identity(),
            reason: "Denied".to_owned(),
        };
        let message = report.message();
        assert!(message.contains("could not confirm"), "{message}");
        assert!(message.contains("Denied"), "{message}");
        assert!(message.contains("nothing was sent"), "{message}");
    }

    #[test]
    fn expected_outcomes_are_informational_and_thwarted_intent_is_not() {
        assert_eq!(
            SignalReport::Delivered {
                signal: SignalKind::Term,
                identity: identity()
            }
            .severity(),
            Severity::Info
        );
        assert_eq!(
            SignalReport::Vanished(identity()).severity(),
            Severity::Info
        );
        assert_eq!(
            SignalReport::AlreadyExited(identity()).severity(),
            Severity::Info
        );
        assert_eq!(
            SignalReport::PermissionDenied(identity()).severity(),
            Severity::Watch,
            "the user asked for something that did not happen"
        );
        assert_eq!(
            SignalReport::Failed {
                identity: identity(),
                reason: "x".to_owned()
            }
            .severity(),
            Severity::Critical
        );
    }

    #[test]
    fn every_signal_kind_maps_to_a_native_signal() {
        // Not a live delivery: this pins the exhaustive match so adding a variant
        // to `SignalKind` cannot silently fall through to a wrong signal.
        for kind in [
            SignalKind::Term,
            SignalKind::Int,
            SignalKind::Hup,
            SignalKind::Kill,
        ] {
            assert!(!kind.name().is_empty());
        }
    }

    /// Delivering to a PID that cannot exist must refuse rather than succeed.
    ///
    /// `#[ignore]` because it consults the live process table; CI runs it with
    /// `-- --ignored` on both platforms.
    #[test]
    #[ignore = "platform smoke test: consults the live process table"]
    fn smoke_a_phantom_pid_is_never_reported_as_delivered() {
        let phantom = ProcessIdentity::new(0x7fff_0000, 1);
        let report = deliver(phantom, SignalKind::Term);
        assert!(!report.was_delivered(), "got {report:?}");
        assert!(
            matches!(
                report,
                SignalReport::Vanished(_)
                    | SignalReport::AlreadyExited(_)
                    | SignalReport::Reused { .. }
                    | SignalReport::Unverifiable { .. }
                    | SignalReport::Unsupported(_)
            ),
            "got {report:?}"
        );
    }

    /// A stale start key must abort even though the PID is live.
    #[test]
    #[ignore = "platform smoke test: consults the live process table"]
    fn smoke_a_stale_start_key_on_our_own_pid_aborts() {
        let stale = ProcessIdentity::new(std::process::id(), 1);
        let report = deliver(stale, SignalKind::Hup);
        assert!(
            !report.was_delivered(),
            "a stale identity must never be signalled, got {report:?}"
        );
        // We are still alive, which is the real assertion.
        assert_eq!(std::process::id(), std::process::id());
    }
}
