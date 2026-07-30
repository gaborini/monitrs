//! Renice: `setpriority(2)`, with the process identity revalidated first.
//!
//! §6.2 binds `R` to "propose renice dialog where supported", and §15.1 decides what
//! "supported" is allowed to mean. This module is deliberately *not* under `linux/`
//! or `macos/`: `setpriority(2)` is POSIX, its `PRIO_PROCESS` selector and its
//! `-20..=19` range are identical on both targets, and writing the call twice would
//! mean two chances to get the safety rules wrong. What genuinely differs between
//! the platforms is how an identity is rechecked, and that is delegated to the
//! platform module that already does it for signals rather than reimplemented here.
//!
//! # The rules this module exists to enforce
//!
//! 1. **Revalidate `(pid, start_key)` immediately before the write.** A PID alone is
//!    not a process identity and a process can vanish between any two reads (§26), so
//!    renicing a recycled PID is the same class of mistake as signalling one: the
//!    wrong process is affected, and the user is told the right one was. [`renice`] is
//!    the only way to reach `setpriority`, and it calls [`confirm`] one statement
//!    before the write, with nothing in between (§6.2, §15.1).
//! 2. **Never escalate.** Lowering a nice value needs privileges. `EPERM` is a
//!    distinct outcome — [`ReniceOutcome::NotPermitted`], carrying which direction
//!    was attempted — so the UI can explain the refusal instead of merely failing.
//!    Nothing here re-executes, prompts for a password, or mentions `sudo` (§15.1).
//! 3. **Refuse an out-of-range value before any syscall.** [`Nice`] is the only way
//!    to name a value the write accepts, and it cannot be constructed outside
//!    [`MIN_NICE`]`..=`[`MAX_NICE`]; the refusal names the range.
//! 4. **A zombie is not renicable.** The kernel will often *accept* the call — the
//!    task struct survives until the parent reaps it — and change nothing
//!    observable. §15.1 requires saying so rather than pretending to act.
//! 5. **Say in advance what would happen.** [`dry_run`] answers "would this even be
//!    permitted?" without writing anything, so the confirmation dialog §6.2
//!    describes can explain a refusal before the user commits to it.
//!
//! # The forecast advises; the kernel decides
//!
//! [`forecast`] is a pure function over what the caller already knows, and
//! [`dry_run`] is that function over freshly read values. Neither is allowed to veto
//! a write, and [`renice`] does not consult them: three real mechanisms let a
//! lowering that "should" fail succeed, and a monitor that refused locally would be
//! denying a capability the machine actually grants.
//!
//! * Linux `RLIMIT_NICE` lets an unprivileged process lower its niceness down to
//!   `20 - rlim_cur`.
//! * `CAP_SYS_NICE` grants the privilege without a uid of 0.
//! * POSIX compares the caller's effective uid against the target's *real or
//!   effective* uid, while a snapshot carries only one of the two.
//!
//! So the dry run is advisory by construction, the `EPERM` outcome is authoritative,
//! and the two are separate types so neither can be mistaken for the other.
//!
//! # The range and the dialog
//!
//! [`MIN_NICE`] and [`MAX_NICE`] are `-20` and `19`, which agree with the `MIN_NICE`
//! and `MAX_NICE` that `monitrs-tui`'s `app/overlay.rs` offers in the renice dialog.
//! §10.1 forbids this crate from depending on the UI, so the agreement cannot be a
//! type: it is pinned by `the_range_agrees_with_the_dialog_the_ui_offers`, which
//! names that file so a change on either side is a failing test rather than a
//! dialog that offers a value the collector refuses.
//!
//! # Why `getpriority`, `setpriority`, and the errno slot are declared here
//!
//! `libc` is an *optional, macOS-only* dependency of this crate, so a
//! platform-neutral module cannot rely on it being present at all. It is also not
//! uniform where it is present: `libc` declares `setpriority`'s first parameter as
//! `__priority_which_t` (a `c_uint`) on glibc and as `c_int` on musl, so code
//! written against one Linux target fails to compile for the other — and CI builds
//! both. The ABI, by contrast, is fixed and identical everywhere this module runs:
//! `int setpriority(int, id_t, int)` with `id_t` a 32-bit unsigned integer, and a C
//! enum passed exactly as an `int`. One hand-written declaration is therefore both
//! shorter and more portable than the alternative. `crate::linux::signal` declares
//! `kill(2)` for the same reason.
//!
//! Every unsafe block below names its invariant (§15.3). None of them dereferences a
//! pointer the kernel wrote, because none of these calls takes one.

use core::ffi::c_int;

use monitrs_core::model::{
    CapabilityState, MetricState, ProcessIdentity, ProcessSnapshot, ProcessState,
};

/// The lowest nice value POSIX defines: the greediest a process can ask to be.
///
/// Reaching it needs privileges monitrs will not escalate to (§15.1), so it is a
/// value the dialog may *ask* for and the kernel may refuse.
pub const MIN_NICE: i8 = -20;

/// The highest nice value POSIX defines: the nicest a process can be.
///
/// Moving towards it is always permitted for a process we own, which is why it is
/// the direction the live tests exercise.
pub const MAX_NICE: i8 = 19;

/// Whether this build can renice at all.
///
/// False on a target without `setpriority(2)`, and false on macOS without the
/// `macos-native` feature — not because the write is unavailable there, but because
/// the identity revalidation §15.1 requires is not compiled in, and a renice that
/// cannot revalidate must not be offered.
pub const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-native")
));

/// `PRIO_PROCESS`: renice one process rather than a group or a user.
///
/// Zero in `<sys/resource.h>` on Linux and on macOS. The group and user selectors
/// are deliberately not modelled: §15.1 forbids acting on a process tree in v1, and
/// a monitor has no business renicing every process of a uid.
const PRIO_PROCESS: c_int = 0;

/// `EPERM`, `ESRCH`, and `EACCES` have these values on both Linux and macOS.
///
/// Pinned as constants rather than pulled from `libc`, which this module cannot
/// depend on, and checked against `libc` by a macOS-only test.
const EPERM: c_int = 1;
/// `ESRCH`: no such process.
const ESRCH: c_int = 3;
/// `EACCES`: permission denied. `setpriority` reports `EPERM`, but a kernel
/// answering the ownership check with `EACCES` must read as the same refusal.
const EACCES: c_int = 13;

/// The capability state to report for renice (§4, §6.2).
///
/// [`CapabilityState::PermissionDenied`] is deliberately never returned: raising the
/// nice value of our own processes always works, so the capability is present. Which
/// *individual* renice needs privileges is a per-attempt question, answered by
/// [`dry_run`] and by [`ReniceOutcome::NotPermitted`] — reporting the whole
/// capability as privilege-denied would put a misleading hint on the Inspect screen.
#[must_use]
pub const fn capability_state() -> CapabilityState {
    if SUPPORTED {
        CapabilityState::Available
    } else {
        CapabilityState::Unsupported
    }
}

/// A value outside [`MIN_NICE`]`..=`[`MAX_NICE`], refused before any syscall.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutOfRange {
    /// The value that was refused, kept so the message can quote it back.
    pub requested: i32,
}

impl OutOfRange {
    /// The refusal, naming the range as well as the value.
    ///
    /// "invalid nice value" would leave the user guessing at what is valid, so the
    /// bounds are always spelled out.
    #[must_use]
    pub fn message(self) -> String {
        format!(
            "nice must be between {MIN_NICE} and {MAX_NICE}; {} is outside that range",
            self.requested
        )
    }
}

/// A nice value that has been range-checked.
///
/// The only type [`renice`] will write, so the range check cannot be skipped by any
/// call path — the same structural trick `crate::linux::signal::SignalDecision` uses
/// to make revalidation unskippable.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Nice(i8);

impl Nice {
    /// Validates `requested` against [`MIN_NICE`]`..=`[`MAX_NICE`].
    ///
    /// Takes an `i32` rather than an `i8` because that is what a config file, a
    /// command palette, or `setpriority` itself can carry, and a value that does not
    /// even fit an `i8` must be refused with the same message as `20` rather than
    /// wrapping into range.
    pub fn new(requested: i32) -> Result<Self, OutOfRange> {
        match i8::try_from(requested) {
            Ok(value) if (MIN_NICE..=MAX_NICE).contains(&value) => Ok(Self(value)),
            _ => Err(OutOfRange { requested }),
        }
    }

    /// The validated value.
    #[must_use]
    pub const fn get(self) -> i8 {
        self.0
    }
}

/// Which way a renice moves the value, which is what decides whether it needs
/// privileges.
///
/// The vocabulary is deliberately not "up" and "down": a *higher* nice value means a
/// *lower* claim on the CPU, and every second reader gets that backwards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NiceDirection {
    /// The nice value rises: the process asks for less CPU.
    ///
    /// Permitted without privileges for a process we own, which is why this is the
    /// direction §15.1 lets monitrs offer unconditionally.
    Nicer,
    /// The nice value falls: the process asks for more CPU.
    ///
    /// Needs `CAP_SYS_NICE`, a uid of 0, or headroom in `RLIMIT_NICE`. monitrs never
    /// acquires any of the three (§15.1).
    MoreDemanding,
    /// The value does not move.
    Unchanged,
}

impl NiceDirection {
    /// Which way a move from `from` to `to` goes.
    #[must_use]
    pub const fn of(from: i8, to: i8) -> Self {
        if to > from {
            Self::Nicer
        } else if to < from {
            Self::MoreDemanding
        } else {
            Self::Unchanged
        }
    }

    /// Whether this direction needs privileges monitrs does not have.
    #[must_use]
    pub const fn needs_privilege(self) -> bool {
        matches!(self, Self::MoreDemanding)
    }
}

/// Why a renice would be refused for want of privileges.
///
/// Two different sentences for the user: one is about the process, the other about
/// the direction of the change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivilegeReason {
    /// The process belongs to somebody else and we are not root.
    AnotherUsersProcess {
        /// The uid that owns it, so the message can name it.
        owner: u32,
    },
    /// Our own process, but the value would fall.
    LoweringNiceness,
}

impl PrivilegeReason {
    /// The refusal, phrased so the user knows what would fix it and that monitrs
    /// will not do it for them (§15.1).
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::AnotherUsersProcess { owner } => format!(
                "the process belongs to uid {owner}; changing its priority needs elevated \
                 privileges, which monitrs never acquires"
            ),
            Self::LoweringNiceness => "lowering a nice value needs elevated privileges, which \
                 monitrs never acquires; raising it is always permitted"
                .to_owned(),
        }
    }
}

/// The effective privileges of the calling process, as far as they matter here.
///
/// A value rather than a query so [`forecast`] can stay pure and be tested for every
/// combination of uid and direction without being root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Privileges {
    /// Our effective uid, or `None` where it could not be asked for.
    euid: Option<u32>,
}

impl Privileges {
    /// The calling process's effective uid.
    ///
    /// An effect-side call: it reads OS state, so it belongs on the runtime's side of
    /// §10.5 rather than in a reducer or a render pass.
    #[must_use]
    pub fn current() -> Self {
        Self {
            euid: effective_uid(),
        }
    }

    /// A specific effective uid.
    #[must_use]
    pub const fn with_euid(euid: u32) -> Self {
        Self { euid: Some(euid) }
    }

    /// Privileges that could not be determined.
    ///
    /// Distinct from "uid 0" and from "not root": an unknown euid makes the forecast
    /// [`ReniceForecast::Undecidable`] rather than a guess in either direction (§4 —
    /// unknown is never a value).
    #[must_use]
    pub const fn unknown() -> Self {
        Self { euid: None }
    }

    /// Whether the effective uid is known at all.
    #[must_use]
    pub const fn is_known(self) -> bool {
        self.euid.is_some()
    }

    /// The effective uid, where it could be read.
    ///
    /// Exposed so a caller describing *its own* process can fill
    /// [`ReniceTarget::owner_uid`] without a second syscall.
    #[must_use]
    pub const fn euid(self) -> Option<u32> {
        self.euid
    }

    /// Whether we are root, and may therefore move any value in any direction.
    #[must_use]
    pub const fn is_root(self) -> bool {
        matches!(self.euid, Some(0))
    }

    /// Whether `uid` is us.
    #[must_use]
    pub const fn owns(self, uid: u32) -> bool {
        match self.euid {
            Some(mine) => mine == uid,
            None => false,
        }
    }
}

/// What the caller knows about the process it wants to renice.
///
/// Everything except the identity is what the *snapshot the user acted on* said, so
/// every optional field means "not measured" and never zero (§4). The identity is the
/// one field that is not taken on trust: [`renice`] rechecks it against the OS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReniceTarget {
    /// The identity the user confirmed, rechecked immediately before the write.
    pub identity: ProcessIdentity,
    /// The scheduling state as of that snapshot.
    ///
    /// A zombie is refused on the strength of this alone, before any syscall, because
    /// the kernel would accept the write and change nothing observable.
    pub state: ProcessState,
    /// The nice value as of that snapshot, where the platform reported one.
    pub current_nice: Option<i8>,
    /// The uid that owns the process, where the platform reported one.
    pub owner_uid: Option<u32>,
}

impl ReniceTarget {
    /// A target described only by the two facts the safety rules need.
    ///
    /// The forecast will be [`ReniceForecast::Undecidable`] for anything that depends
    /// on the current value or the owner, which is the honest answer when they are
    /// not known.
    #[must_use]
    pub const fn new(identity: ProcessIdentity, state: ProcessState) -> Self {
        Self {
            identity,
            state,
            current_nice: None,
            owner_uid: None,
        }
    }

    /// A target built from a process row and the niceness the detail pass measured.
    ///
    /// The two arguments are separate because the model keeps them apart:
    /// `ProcessSnapshot` carries the state and the owner of every process on every
    /// tick, while niceness is on `ProcessDetail`, collected on demand for one
    /// process (§8.6). Pass `MetricState::WarmingUp` when no detail has arrived yet.
    ///
    /// A stale value is accepted for both, because a forecast is advisory and a
    /// niceness that was right one tick ago is a far better basis for it than nothing;
    /// the authoritative reading is taken by [`dry_run`] and by [`renice`] itself.
    #[must_use]
    pub fn from_snapshot(process: &ProcessSnapshot, nice: &MetricState<i32>) -> Self {
        Self {
            identity: process.identity,
            state: process.state,
            current_nice: nice
                .displayable()
                .and_then(|(value, _)| i8::try_from(*value).ok()),
            owner_uid: process.user.displayable().map(|(user, _)| user.uid),
        }
    }
}

/// What [`renice`] would do, worked out without writing anything.
///
/// Answers the permission question only. Whether the process is still the one the
/// user selected is [`confirm`]'s answer, and keeping the two apart is what stops one
/// enum from growing two overlapping sets of refusals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReniceForecast {
    /// The value is outside the range. Refused here, before any syscall.
    OutOfRange(OutOfRange),
    /// The PID does not name one specific process, so nothing will be attempted.
    NotASingleProcess,
    /// The process has already exited, so it has no priority left to change.
    AlreadyExited,
    /// The process is already at the requested value.
    Unchanged {
        /// The value it is already at.
        at: i8,
    },
    /// The write should succeed.
    Permitted {
        /// The current value, where it is known.
        from: Option<i8>,
        /// The requested value.
        to: i8,
    },
    /// The write would be refused with `EPERM`, and monitrs will not escalate.
    NotPermitted {
        /// Which obstacle applies.
        reason: PrivilegeReason,
        /// The current value, where it is known.
        from: Option<i8>,
        /// The requested value.
        to: i8,
    },
    /// Not enough is known to say. The write is still allowed to be attempted.
    Undecidable {
        /// The requested value.
        to: i8,
    },
    /// This build cannot renice at all (see [`SUPPORTED`]).
    Unsupported,
}

impl ReniceForecast {
    /// Whether the write is expected to succeed.
    ///
    /// [`Self::Undecidable`] answers `false`: the dialog must not promise something
    /// only the kernel can decide.
    #[must_use]
    pub const fn is_permitted(&self) -> bool {
        matches!(self, Self::Permitted { .. })
    }

    /// Whether attempting the write is worth doing at all.
    ///
    /// True for [`Self::Permitted`], [`Self::NotPermitted`], and
    /// [`Self::Undecidable`]: an attempt that is merely *predicted* to fail must
    /// still be offered, because the prediction can be wrong in the user's favour —
    /// see this module's note on `RLIMIT_NICE`.
    #[must_use]
    pub const fn is_worth_attempting(&self) -> bool {
        matches!(
            self,
            Self::Permitted { .. } | Self::NotPermitted { .. } | Self::Undecidable { .. }
        )
    }

    /// A sentence for the confirmation dialog.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::OutOfRange(refusal) => refusal.message(),
            Self::NotASingleProcess => {
                "that PID does not name a single process, so nothing will be changed".to_owned()
            }
            Self::AlreadyExited => {
                "the process has exited and has no scheduling priority left to change".to_owned()
            }
            Self::Unchanged { at } => format!("already at nice {at}; nothing would change"),
            Self::Permitted { from, to } => match from {
                Some(from) => format!("nice {from} would become {to}"),
                None => format!("nice would become {to}"),
            },
            Self::NotPermitted { reason, .. } => reason.message(),
            Self::Undecidable { to } => format!(
                "whether nice {to} is permitted can only be answered by the kernel; \
                 raising the value always is, lowering it needs privileges"
            ),
            Self::Unsupported => "this build cannot change scheduling priority".to_owned(),
        }
    }
}

/// What [`confirm`] concluded about an identity, in platform-neutral terms.
///
/// Only [`Confirmation::Current`] permits a write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Confirmation {
    /// The PID still refers to the process the user selected.
    Current,
    /// The process no longer exists.
    ///
    /// Expected rather than exceptional (§14.1).
    Vanished,
    /// The PID refers to something else now. **Abort.**
    Changed {
        /// What it refers to now, where the platform can say.
        ///
        /// `None` on macOS: `identity_is_current` answers one bool, exactly as its
        /// `SignalOutcome` folds "gone" into `AlreadyExited`. The safe reading of
        /// `false` is "not the process you selected", and nothing is written either
        /// way, so the missing detail costs a better message and no safety.
        found: Option<ProcessIdentity>,
    },
    /// The process is a zombie: present, but with nothing left to renice.
    AlreadyExited,
    /// The identity could not be rechecked, so nothing may be written.
    ///
    /// Refusing is the only safe direction: an unverifiable identity is exactly the
    /// case a recycled PID hides in.
    Unverifiable {
        /// Why the recheck failed.
        reason: &'static str,
    },
    /// This build has no way to recheck an identity, so it may not write (§15.1).
    Unavailable,
}

/// What happened when a renice was attempted.
///
/// Not a `Result`: several of these are ordinary facts about a process rather than
/// failures, and collapsing them would lose the distinction §14.1 draws between an
/// error and a process that exited (§9.3 requires the same of signals).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReniceOutcome {
    /// The kernel accepted the change.
    Applied {
        /// The value before the write, where `getpriority` could report it.
        from: Option<i8>,
        /// The value that was requested.
        to: i8,
        /// The value the kernel reports afterwards, read back rather than assumed.
        confirmed: Option<i8>,
    },
    /// The value was outside the range. Nothing was attempted.
    OutOfRange(OutOfRange),
    /// The PID does not name one specific process. Nothing was attempted.
    NotASingleProcess,
    /// The process was already gone when the identity was rechecked.
    Vanished,
    /// The PID belonged to a different process by then, so nothing was written.
    ///
    /// The outcome the whole revalidation exists to produce.
    Reused {
        /// The identity the user confirmed.
        requested: ProcessIdentity,
        /// What the PID refers to now, where the platform can say.
        found: Option<ProcessIdentity>,
    },
    /// The process is a zombie, so there was nothing to change.
    AlreadyExited,
    /// The identity could not be rechecked, so nothing was written.
    Unverifiable {
        /// Why the recheck failed.
        reason: &'static str,
    },
    /// `EPERM`: the kernel refused, and monitrs does not escalate (§15.1).
    NotPermitted {
        /// Which direction was attempted, where the previous value was readable.
        ///
        /// This is what lets the UI say *why*: a refused [`NiceDirection::Nicer`]
        /// means the process belongs to somebody else, while a refused
        /// [`NiceDirection::MoreDemanding`] is the ordinary "lowering needs
        /// privileges" case even for our own process.
        attempted: Option<NiceDirection>,
        /// The value that was requested.
        to: i8,
    },
    /// The kernel refused for another reason.
    Failed {
        /// The raw errno, for the log.
        errno: i32,
    },
    /// This build cannot renice (see [`SUPPORTED`]).
    Unsupported,
}

impl ReniceOutcome {
    /// Whether the value actually changed.
    #[must_use]
    pub const fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// Whether this outcome is routine rather than something to warn about.
    ///
    /// A process that exited between the dialog and the write is expected (§14.1); a
    /// reused PID is not — it means monitrs came one step from renicing the wrong
    /// process, and the user should be told.
    #[must_use]
    pub const fn is_expected(&self) -> bool {
        matches!(self, Self::Applied { .. } | Self::Vanished)
    }

    /// A sentence for the status line.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Applied {
                from,
                to,
                confirmed,
            } => applied_message(*from, *to, *confirmed),
            Self::OutOfRange(refusal) => refusal.message(),
            Self::NotASingleProcess => {
                "refused: that PID does not name a single process".to_owned()
            }
            Self::Vanished => "the process had already exited; nothing was changed".to_owned(),
            Self::Reused { requested, found } => match found {
                Some(found) => format!(
                    "aborted: PID {} now belongs to a different process (start key {} not {}); \
                     nothing was changed",
                    requested.pid, found.start_key, requested.start_key
                ),
                None => format!(
                    "aborted: PID {} no longer refers to the process that was selected; \
                     nothing was changed",
                    requested.pid
                ),
            },
            Self::AlreadyExited => {
                "the process is a zombie: there is no scheduling priority left to change".to_owned()
            }
            Self::Unverifiable { reason } => format!(
                "could not confirm the process is still the same one ({reason}); \
                 nothing was changed"
            ),
            Self::NotPermitted { attempted, to } => match attempted {
                Some(direction) if direction.needs_privilege() => {
                    PrivilegeReason::LoweringNiceness.message()
                }
                _ => format!(
                    "not permitted to set nice {to}; the process belongs to another user, and \
                     monitrs never acquires elevated privileges"
                ),
            },
            Self::Failed { errno } => format!("setting the nice value failed with errno {errno}"),
            Self::Unsupported => "this build cannot change scheduling priority".to_owned(),
        }
    }
}

/// The sentence for a successful write, reporting what the kernel says rather than
/// what was asked for.
///
/// A read-back that disagrees with the request is worth showing: it is the only way a
/// user would learn that something else — an autogroup, a container runtime, a
/// scheduling policy — moved the value again.
fn applied_message(from: Option<i8>, to: i8, confirmed: Option<i8>) -> String {
    match (from, confirmed) {
        (_, Some(confirmed)) if confirmed != to => {
            format!("nice {to} was requested; the kernel reports {confirmed}")
        }
        (Some(from), _) => format!("nice {from} is now {to}"),
        (None, _) => format!("nice is now {to}"),
    }
}

/// Whether a process in this state still has a scheduling priority to change.
///
/// A zombie is neither signalable nor renicable, for the same reason: there is no
/// live task behind the PID. `setpriority` will often return success anyway, because
/// the task struct survives until the parent reaps it, which is precisely why this
/// check happens before the syscall and not after it (§15.1).
const fn has_priority(state: ProcessState) -> bool {
    !matches!(state, ProcessState::Zombie | ProcessState::Dead)
}

/// The `id_t` to pass to `setpriority`, or `None` if this PID must not be written.
///
/// PID 0 is refused: `setpriority(PRIO_PROCESS, 0, prio)` acts on the **calling**
/// process, so a selected row for PID 0 — which is how macOS reports `kernel_task` —
/// would silently renice monitrs itself instead of the process the user chose. A
/// value too large for a `pid_t` cannot be a live process either, so it is refused
/// rather than handed to the kernel.
fn renicable_pid(pid: u32) -> Option<u32> {
    match i32::try_from(pid) {
        Ok(value) if value > 0 => Some(pid),
        _ => None,
    }
}

/// What [`renice`] would do, from what the caller already knows. Pure.
///
/// Advisory: see this module's note on why the kernel, not this function, decides.
#[must_use]
pub fn forecast(target: &ReniceTarget, requested: i32, privileges: Privileges) -> ReniceForecast {
    let to = match Nice::new(requested) {
        Ok(nice) => nice.get(),
        Err(refusal) => return ReniceForecast::OutOfRange(refusal),
    };
    if renicable_pid(target.identity.pid).is_none() {
        return ReniceForecast::NotASingleProcess;
    }
    if !has_priority(target.state) {
        return ReniceForecast::AlreadyExited;
    }
    let from = target.current_nice;
    if from == Some(to) {
        return ReniceForecast::Unchanged { at: to };
    }
    if privileges.is_root() {
        // Root may move any process's value in either direction, so neither the owner
        // nor the current value needs to be known to answer.
        return ReniceForecast::Permitted { from, to };
    }
    if !privileges.is_known() {
        return ReniceForecast::Undecidable { to };
    }
    match target.owner_uid {
        Some(owner) if !privileges.owns(owner) => {
            return ReniceForecast::NotPermitted {
                reason: PrivilegeReason::AnotherUsersProcess { owner },
                from,
                to,
            };
        }
        // Ownership decides the refusal on its own, so an unknown owner cannot be
        // resolved by looking at the direction.
        None => return ReniceForecast::Undecidable { to },
        Some(_) => {}
    }
    match from {
        Some(from_value) if to < from_value => ReniceForecast::NotPermitted {
            reason: PrivilegeReason::LoweringNiceness,
            from,
            to,
        },
        Some(_) => ReniceForecast::Permitted { from, to },
        None => ReniceForecast::Undecidable { to },
    }
}

/// The dry run: what [`renice`] would do *now*, without writing anything.
///
/// Differs from [`forecast`] in reading the current value and our own euid from the
/// OS instead of taking them from a snapshot that may be a tick old — a forecast
/// about a stale value is a forecast about the wrong state. Read-only, and therefore
/// effect-side rather than reducer-side (§10.5).
#[must_use]
pub fn dry_run(target: &ReniceTarget, requested: i32) -> ReniceForecast {
    if !SUPPORTED {
        return ReniceForecast::Unsupported;
    }
    let mut refreshed = *target;
    if let Some(pid) = renicable_pid(target.identity.pid)
        && let Some(nice) = live_nice(pid)
    {
        refreshed.current_nice = Some(nice);
    }
    forecast(&refreshed, requested, Privileges::current())
}

/// Rechecks `(pid, start_key)` against the OS as it is right now.
///
/// Public because the confirmation dialog §6.2 describes wants to show the process's
/// details at the moment of asking rather than at the moment of selection. Note that
/// a `Current` answer is only true for the instant it was produced: no Unix offers
/// "write if this is still the same process", so the window is made microseconds
/// wide by [`renice`] rather than closed.
#[must_use]
pub fn confirm(identity: ProcessIdentity) -> Confirmation {
    confirm_identity(identity)
}

/// Renices `target` to `requested`, revalidating the identity immediately first.
///
/// The order is the whole point of the function: range check, PID check, zombie
/// check, previous value, **revalidate**, write. The revalidation is the last thing
/// before `setpriority`, so §6.2's "re-read the process identity immediately before
/// executing an action" is structural rather than a comment (§15.1).
#[must_use]
pub fn renice(target: &ReniceTarget, requested: i32) -> ReniceOutcome {
    let to = match Nice::new(requested) {
        Ok(nice) => nice,
        Err(refusal) => return ReniceOutcome::OutOfRange(refusal),
    };
    let Some(pid) = renicable_pid(target.identity.pid) else {
        return ReniceOutcome::NotASingleProcess;
    };
    if !has_priority(target.state) {
        return ReniceOutcome::AlreadyExited;
    }

    // Read the previous value *before* revalidating, so that nothing at all happens
    // between the identity check and the write.
    let previous = live_nice(pid);

    match confirm_identity(target.identity) {
        Confirmation::Current => {}
        Confirmation::Vanished => return ReniceOutcome::Vanished,
        Confirmation::Changed { found } => {
            return ReniceOutcome::Reused {
                requested: target.identity,
                found,
            };
        }
        Confirmation::AlreadyExited => return ReniceOutcome::AlreadyExited,
        Confirmation::Unverifiable { reason } => return ReniceOutcome::Unverifiable { reason },
        Confirmation::Unavailable => return ReniceOutcome::Unsupported,
    }

    match write_nice(pid, to) {
        Ok(()) => ReniceOutcome::Applied {
            from: previous,
            to: to.get(),
            confirmed: live_nice(pid),
        },
        Err(errno) => outcome_from_errno(errno, previous, to.get()),
    }
}

/// Maps a failed `setpriority` onto an outcome.
///
/// `EPERM` and `EACCES` are one outcome with the attempted direction attached, which
/// is what §15.1 means by reporting a permission failure clearly: the user learns
/// whether the obstacle is the process or the direction, and monitrs offers no way
/// around either.
fn outcome_from_errno(errno: i32, previous: Option<i8>, to: i8) -> ReniceOutcome {
    match errno {
        EPERM | EACCES => ReniceOutcome::NotPermitted {
            attempted: previous.map(|from| NiceDirection::of(from, to)),
            to,
        },
        // The residual race no amount of checking removes: the process exited between
        // revalidation and the write.
        ESRCH => ReniceOutcome::Vanished,
        other => ReniceOutcome::Failed { errno: other },
    }
}

/// Reads the process's current nice value, or `None` if it could not be read.
///
/// `None` is "unknown", never zero (§4): a process at nice 0 and a process whose
/// priority could not be read must not look the same to the forecast.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn live_nice(pid: u32) -> Option<i8> {
    sys::read_priority(pid)
        .ok()
        .and_then(|value| i8::try_from(value).ok())
}

/// No `getpriority` on this target.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn live_nice(_pid: u32) -> Option<i8> {
    None
}

/// Writes the nice value.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_nice(pid: u32, nice: Nice) -> Result<(), i32> {
    sys::write_priority(pid, c_int::from(nice.get()))
}

/// No `setpriority` on this target.
///
/// Unreachable in practice: [`confirm_identity`] answers [`Confirmation::Unavailable`]
/// on every target without a revalidation path, and [`renice`] returns before it gets
/// here. It exists so the module compiles everywhere rather than being absent on some
/// targets, and it fails rather than silently claiming success.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn write_nice(_pid: u32, _nice: Nice) -> Result<(), i32> {
    Err(EPERM)
}

/// Our effective uid, or `None` where it cannot be asked for.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn effective_uid() -> Option<u32> {
    Some(sys::effective_uid())
}

/// No `geteuid` on this target.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn effective_uid() -> Option<u32> {
    None
}

/// Rechecks the identity from a freshly read `/proc/<pid>/stat` (§9.2).
///
/// Delegates to `crate::linux::signal::revalidate`, which the signal path already
/// trusts, rather than parsing `stat` a third time: the interesting cases — a PID
/// recycled inside one second, a name full of parentheses, a truncated read — are
/// covered by its fixture tests, and a second implementation would only be a second
/// thing to get wrong.
#[cfg(target_os = "linux")]
fn confirm_identity(identity: ProcessIdentity) -> Confirmation {
    use crate::linux::{ProcRoot, ReadFailure, revalidate};

    let root = ProcRoot::live();
    let bytes = root.read_pid(identity.pid, "stat");
    // `ReadFailure` is `Copy`, so the failure travels by value while the borrow of the
    // buffer ends with this call.
    let fresh: Result<&[u8], ReadFailure> = bytes.as_deref().map_err(|failure| *failure);
    confirmation_from(revalidate(identity, INERT_SIGNAL, fresh))
}

/// The signal handed to `revalidate` on Linux, which never delivers anything.
///
/// `revalidate` is a pure function: it inspects `stat` and returns a decision, and
/// nothing in this module passes that decision to a sink. The argument is therefore
/// inert, and `SIGCONT` is chosen as the value that would do the least harm if some
/// future refactor made it live — a resumed process, not a dead one.
#[cfg(any(target_os = "linux", test))]
const INERT_SIGNAL: crate::linux::LinuxSignal = crate::linux::LinuxSignal::Cont;

/// Maps the Linux signal path's decision onto a renice confirmation.
///
/// Compiled off Linux for the tests only, which exercise this mapping from the
/// checked-in `/proc` fixtures on any host — the reason the Linux parsers are not
/// gated to Linux in the first place (§17.2).
#[cfg(any(target_os = "linux", test))]
fn confirmation_from(decision: crate::linux::SignalDecision) -> Confirmation {
    use crate::linux::SignalDecision;

    match decision {
        SignalDecision::Deliver { .. } => Confirmation::Current,
        SignalDecision::Vanished(_) => Confirmation::Vanished,
        SignalDecision::Reused { found, .. } => Confirmation::Changed { found: Some(found) },
        SignalDecision::AlreadyExited(_) => Confirmation::AlreadyExited,
        SignalDecision::Unverifiable { failure, .. } => Confirmation::Unverifiable {
            reason: failure.describe(),
        },
    }
}

/// Rechecks the identity against `kern.proc.pid` (§9.3).
///
/// `identity_is_current` is the macOS signal path's own check, reused here for the
/// same reason as on Linux. It answers one bool, so a `false` becomes
/// [`Confirmation::Changed`] with no detail: vanished, recycled, and unreadable are
/// indistinguishable through that API, and all three mean the same thing to a writer
/// — do not write.
#[cfg(all(target_os = "macos", feature = "macos-native"))]
fn confirm_identity(identity: ProcessIdentity) -> Confirmation {
    if crate::macos::identity_is_current(identity) {
        Confirmation::Current
    } else {
        Confirmation::Changed { found: None }
    }
}

/// No revalidation on this target, so no write is permitted (§15.1).
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-native")
)))]
fn confirm_identity(_identity: ProcessIdentity) -> Confirmation {
    Confirmation::Unavailable
}

/// The two POSIX calls, the errno slot, and `geteuid`.
///
/// Declared rather than imported from `libc`: see the module documentation for why
/// that is both shorter and more portable here. Everything in this module that is
/// `unsafe` is in this submodule, and each block names its invariant (§15.3).
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod sys {
    use core::ffi::c_int;

    unsafe extern "C" {
        /// `int getpriority(int which, id_t who)`.
        ///
        /// `id_t` is a 32-bit unsigned integer on both platforms, and glibc's
        /// `__priority_which_t` is a C enum, which is passed exactly as an `int`.
        fn getpriority(which: c_int, who: u32) -> c_int;

        /// `int setpriority(int which, id_t who, int prio)`.
        fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;

        /// `uid_t geteuid(void)`. Cannot fail, per POSIX.
        fn geteuid() -> u32;

        /// glibc's and musl's accessor for this thread's `errno`.
        #[cfg(target_os = "linux")]
        #[link_name = "__errno_location"]
        fn errno_location() -> *mut c_int;

        /// libSystem's accessor for this thread's `errno`.
        #[cfg(target_os = "macos")]
        #[link_name = "__error"]
        fn errno_location() -> *mut c_int;
    }

    /// Sets this thread's `errno` to 0.
    fn clear_errno() {
        // SAFETY: the accessor returns a pointer to this thread's own errno slot,
        // which the C library guarantees is valid for the life of the thread. The
        // write is a plain `int` store through a pointer that is not retained, and
        // no other thread can observe this thread's slot.
        unsafe { *errno_location() = 0 }
    }

    /// This thread's `errno`, or 0 if it could not be read.
    ///
    /// Read through the standard library, which reads the same slot the accessor
    /// above points at. Zero means "no error", which is exactly how a 0 from a
    /// missing raw error must be interpreted.
    fn last_errno() -> c_int {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// Reads `pid`'s nice value.
    ///
    /// `getpriority` returns `-1` both for a process at nice -1 and for a failure, so
    /// POSIX requires `errno` to be cleared before the call and inspected after it.
    /// This is the classic mistake with the API in both directions: treating every
    /// `-1` as an error reports a legitimately renice'd process as unreadable, and
    /// treating every `-1` as a value reports a failure as "nice -1".
    pub(super) fn read_priority(pid: u32) -> Result<c_int, c_int> {
        clear_errno();
        // SAFETY: `getpriority` takes two integers by value, dereferences nothing,
        // and writes nothing but `errno`. It cannot unwind, so there is no
        // cross-language panic. The result is validated by the caller.
        let value = unsafe { getpriority(super::PRIO_PROCESS, pid) };
        if value == -1 {
            let errno = last_errno();
            if errno != 0 {
                return Err(errno);
            }
        }
        Ok(value)
    }

    /// Sets `pid`'s nice value.
    ///
    /// Unlike `getpriority`, the return value is unambiguous — 0 or -1 — so `errno`
    /// only has to be read on failure and does not have to be cleared first.
    pub(super) fn write_priority(pid: u32, nice: c_int) -> Result<(), c_int> {
        // SAFETY: `setpriority` takes three integers by value and dereferences
        // nothing. `pid` was checked by `super::renicable_pid` to be a strictly
        // positive process id — never 0, which would renice this process — and the
        // identity behind it was revalidated by `super::renice` immediately before
        // this call. `nice` came from a `super::Nice`, so it is within -20..=19.
        let result = unsafe { setpriority(super::PRIO_PROCESS, pid, nice) };
        if result == 0 {
            return Ok(());
        }
        Err(last_errno())
    }

    /// This process's effective uid.
    pub(super) fn effective_uid() -> u32 {
        // SAFETY: `geteuid` takes no arguments, touches no memory, and cannot fail.
        unsafe { geteuid() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity the `simple` `/proc` fixture describes.
    const FIXTURE_PID: u32 = 4_242;
    /// Its start key, in clock ticks.
    const FIXTURE_START_KEY: u64 = 88_213_700;

    /// A target for a live, unremarkable process owned by uid 1000 at nice 0.
    fn target() -> ReniceTarget {
        ReniceTarget {
            identity: ProcessIdentity::new(FIXTURE_PID, FIXTURE_START_KEY),
            state: ProcessState::Sleeping,
            current_nice: Some(0),
            owner_uid: Some(1_000),
        }
    }

    /// Us, for a target owned by uid 1000.
    fn unprivileged() -> Privileges {
        Privileges::with_euid(1_000)
    }

    #[test]
    fn the_range_agrees_with_the_dialog_the_ui_offers() {
        // `monitrs-tui`'s `src/app/overlay.rs` declares `MIN_NICE: i8 = -20` and
        // `MAX_NICE: i8 = 19`, and its dialog clamps the user's choice to that range.
        // §10.1 forbids this crate from depending on the UI, so the agreement is
        // pinned here as literals: if either side moves, this test fails and names the
        // file to look at. They agree today.
        assert_eq!(MIN_NICE, -20);
        assert_eq!(MAX_NICE, 19);
    }

    #[test]
    fn every_value_in_the_posix_range_validates_and_nothing_outside_it_does() {
        for value in i32::from(MIN_NICE)..=i32::from(MAX_NICE) {
            let nice = Nice::new(value).expect("inside the range");
            assert_eq!(i32::from(nice.get()), value);
        }
        for value in [
            i32::from(MIN_NICE) - 1,
            i32::from(MAX_NICE) + 1,
            -100,
            100,
            i32::MIN,
            i32::MAX,
        ] {
            let refusal = Nice::new(value).expect_err("outside the range");
            assert_eq!(refusal.requested, value);
        }
    }

    #[test]
    fn the_out_of_range_refusal_names_the_range_as_well_as_the_value() {
        let message = OutOfRange { requested: 25 }.message();
        assert!(message.contains("-20"), "{message}");
        assert!(message.contains("19"), "{message}");
        assert!(message.contains("25"), "{message}");
    }

    #[test]
    fn a_value_outside_the_range_is_refused_before_anything_else_is_checked() {
        // PID 0 would be refused too, and the process is a zombie: whichever check
        // runs first decides the answer, and the range check must, because it is the
        // only one that needs no syscall at all.
        let doomed = ReniceTarget {
            identity: ProcessIdentity::new(0, 1),
            state: ProcessState::Zombie,
            ..target()
        };
        assert_eq!(
            renice(&doomed, 99),
            ReniceOutcome::OutOfRange(OutOfRange { requested: 99 })
        );
        assert_eq!(
            forecast(&doomed, 99, unprivileged()),
            ReniceForecast::OutOfRange(OutOfRange { requested: 99 })
        );
    }

    #[test]
    fn pid_zero_is_refused_because_setpriority_would_renice_monitrs_itself() {
        // `setpriority(PRIO_PROCESS, 0, prio)` means "the calling process". macOS
        // reports kernel_task as PID 0, so this is a row a user can select.
        assert_eq!(renicable_pid(0), None);
        assert_eq!(renicable_pid(1), Some(1));
        assert_eq!(renicable_pid(u32::MAX), None, "cannot be a pid_t");
        assert_eq!(renicable_pid(0x8000_0000), None);

        let kernel_task = ReniceTarget {
            identity: ProcessIdentity::new(0, 1),
            ..target()
        };
        assert_eq!(renice(&kernel_task, 5), ReniceOutcome::NotASingleProcess);
        assert_eq!(
            forecast(&kernel_task, 5, unprivileged()),
            ReniceForecast::NotASingleProcess
        );
    }

    #[test]
    fn a_zombie_is_refused_before_any_syscall_rather_than_pretended_at() {
        // The PID here cannot exist, so if the state check did not come first the
        // answer would be `Vanished` (Linux) or `Reused` (macOS). It is
        // `AlreadyExited` on every platform, which is what proves the ordering — and
        // §15.1's requirement that an already-exited process is reported honestly.
        for state in [ProcessState::Zombie, ProcessState::Dead] {
            let zombie = ReniceTarget {
                identity: ProcessIdentity::new(0x7fff_0000, 1),
                state,
                ..target()
            };
            assert_eq!(renice(&zombie, 5), ReniceOutcome::AlreadyExited);
            assert_eq!(
                forecast(&zombie, 5, unprivileged()),
                ReniceForecast::AlreadyExited
            );
        }
    }

    #[test]
    fn the_states_with_no_priority_are_exactly_the_states_with_no_signal() {
        // Both questions are "is there a live task behind this PID", so the two
        // predicates must agree; if a future state made them differ, that is a
        // deliberate decision to make here rather than a silent divergence.
        for state in [
            ProcessState::Running,
            ProcessState::Sleeping,
            ProcessState::UninterruptibleSleep,
            ProcessState::Zombie,
            ProcessState::Stopped,
            ProcessState::Traced,
            ProcessState::Idle,
            ProcessState::Dead,
            ProcessState::Unknown,
        ] {
            assert_eq!(
                has_priority(state),
                state.is_signalable(),
                "{state:?} disagrees"
            );
        }
    }

    #[test]
    fn raising_the_nice_value_of_our_own_process_needs_no_privileges() {
        assert_eq!(
            forecast(&target(), 5, unprivileged()),
            ReniceForecast::Permitted {
                from: Some(0),
                to: 5
            }
        );
        let outcome = forecast(&target(), 5, unprivileged());
        assert!(outcome.is_permitted());
        assert!(outcome.is_worth_attempting());
        assert!(outcome.message().contains('5'), "{}", outcome.message());
    }

    #[test]
    fn lowering_the_nice_value_is_forecast_as_needing_privileges() {
        let outcome = forecast(&target(), -5, unprivileged());
        assert_eq!(
            outcome,
            ReniceForecast::NotPermitted {
                reason: PrivilegeReason::LoweringNiceness,
                from: Some(0),
                to: -5
            }
        );
        assert!(!outcome.is_permitted());
        assert!(
            outcome.is_worth_attempting(),
            "RLIMIT_NICE can permit it, so the attempt must still be offered"
        );
        let message = outcome.message();
        assert!(message.contains("privileges"), "{message}");
        assert!(message.contains("raising"), "{message}");
    }

    #[test]
    fn root_may_move_the_value_in_either_direction() {
        let root = Privileges::with_euid(0);
        assert!(root.is_root());
        for requested in [-20, -1, 19] {
            assert_eq!(
                forecast(&target(), requested, root),
                ReniceForecast::Permitted {
                    from: Some(0),
                    to: i8::try_from(requested).expect("in range")
                }
            );
        }
        // Root does not need to know the owner or the current value to be permitted.
        let unknown = ReniceTarget {
            current_nice: None,
            owner_uid: None,
            ..target()
        };
        assert_eq!(
            forecast(&unknown, -10, root),
            ReniceForecast::Permitted {
                from: None,
                to: -10
            }
        );
    }

    #[test]
    fn another_users_process_is_forecast_as_not_permitted_in_either_direction() {
        let theirs = ReniceTarget {
            owner_uid: Some(0),
            ..target()
        };
        for requested in [5, -5] {
            let outcome = forecast(&theirs, requested, unprivileged());
            assert_eq!(
                outcome,
                ReniceForecast::NotPermitted {
                    reason: PrivilegeReason::AnotherUsersProcess { owner: 0 },
                    from: Some(0),
                    to: i8::try_from(requested).expect("in range")
                }
            );
            let message = outcome.message();
            assert!(message.contains("uid 0"), "{message}");
            assert!(message.contains("privileges"), "{message}");
        }
    }

    #[test]
    fn no_forecast_or_outcome_ever_suggests_escalating() {
        // §15.1: monitrs does not escalate, and must not hint at a way to.
        let forecasts = [
            forecast(&target(), 5, unprivileged()),
            forecast(&target(), -5, unprivileged()),
            forecast(
                &ReniceTarget {
                    owner_uid: Some(0),
                    ..target()
                },
                -5,
                unprivileged(),
            ),
            ReniceForecast::Unsupported,
            ReniceForecast::Undecidable { to: 3 },
            ReniceForecast::AlreadyExited,
            ReniceForecast::NotASingleProcess,
            ReniceForecast::Unchanged { at: 0 },
            ReniceForecast::OutOfRange(OutOfRange { requested: 40 }),
        ];
        let outcomes = [
            ReniceOutcome::NotPermitted {
                attempted: Some(NiceDirection::MoreDemanding),
                to: -5,
            },
            ReniceOutcome::NotPermitted {
                attempted: Some(NiceDirection::Nicer),
                to: 5,
            },
            ReniceOutcome::NotPermitted {
                attempted: None,
                to: 5,
            },
        ];
        let messages = forecasts
            .iter()
            .map(ReniceForecast::message)
            .chain(outcomes.iter().map(ReniceOutcome::message));
        for message in messages {
            assert!(!message.is_empty());
            assert!(message.is_ascii(), "{message}");
            let lowered = message.to_lowercase();
            assert!(!lowered.contains("sudo"), "{message}");
            assert!(!lowered.contains("run as root"), "{message}");
        }
    }

    #[test]
    fn an_unknown_current_value_or_owner_or_euid_is_undecidable_rather_than_guessed() {
        // §4: unknown is never a value, and here it is never an answer either.
        let no_nice = ReniceTarget {
            current_nice: None,
            ..target()
        };
        assert_eq!(
            forecast(&no_nice, 5, unprivileged()),
            ReniceForecast::Undecidable { to: 5 }
        );
        let no_owner = ReniceTarget {
            owner_uid: None,
            ..target()
        };
        assert_eq!(
            forecast(&no_owner, 5, unprivileged()),
            ReniceForecast::Undecidable { to: 5 }
        );
        assert_eq!(
            forecast(&target(), 5, Privileges::unknown()),
            ReniceForecast::Undecidable { to: 5 }
        );
        let undecidable = ReniceForecast::Undecidable { to: 5 };
        assert!(!undecidable.is_permitted(), "a guess is not an answer");
        assert!(undecidable.is_worth_attempting());
        assert!(undecidable.message().contains("kernel"));
    }

    #[test]
    fn unknown_privileges_are_neither_root_nor_a_uid() {
        let unknown = Privileges::unknown();
        assert!(!unknown.is_known());
        assert!(!unknown.is_root());
        assert!(!unknown.owns(0));
        assert!(!unknown.owns(1_000));
        assert!(unprivileged().owns(1_000));
        assert!(!unprivileged().owns(0));
        assert!(Privileges::with_euid(0).is_root());
    }

    #[test]
    fn the_current_value_is_reported_as_no_change_rather_than_written_pointlessly() {
        assert_eq!(
            forecast(&target(), 0, unprivileged()),
            ReniceForecast::Unchanged { at: 0 }
        );
        let unchanged = ReniceForecast::Unchanged { at: 0 };
        assert!(!unchanged.is_permitted());
        assert!(
            !unchanged.is_worth_attempting(),
            "there is nothing to attempt"
        );
    }

    #[test]
    fn a_direction_is_named_from_the_movement_and_only_one_needs_privileges() {
        assert_eq!(NiceDirection::of(0, 5), NiceDirection::Nicer);
        assert_eq!(NiceDirection::of(5, 0), NiceDirection::MoreDemanding);
        assert_eq!(NiceDirection::of(-3, -3), NiceDirection::Unchanged);
        assert_eq!(NiceDirection::of(-20, 19), NiceDirection::Nicer);
        assert_eq!(NiceDirection::of(19, -20), NiceDirection::MoreDemanding);
        assert!(NiceDirection::MoreDemanding.needs_privilege());
        assert!(!NiceDirection::Nicer.needs_privilege());
        assert!(!NiceDirection::Unchanged.needs_privilege());
    }

    #[test]
    fn eperm_becomes_a_refusal_that_names_the_direction_and_esrch_does_not() {
        // §15.1 and §9.3: a permission failure and an exited process are different
        // things to tell the user, and neither is a generic "failed".
        assert_eq!(
            outcome_from_errno(EPERM, Some(0), -5),
            ReniceOutcome::NotPermitted {
                attempted: Some(NiceDirection::MoreDemanding),
                to: -5
            }
        );
        assert_eq!(
            outcome_from_errno(EACCES, Some(0), 5),
            ReniceOutcome::NotPermitted {
                attempted: Some(NiceDirection::Nicer),
                to: 5
            }
        );
        assert_eq!(
            outcome_from_errno(ESRCH, Some(0), 5),
            ReniceOutcome::Vanished
        );
        assert_eq!(
            outcome_from_errno(22, Some(0), 5),
            ReniceOutcome::Failed { errno: 22 }
        );
        // An unreadable previous value must not be invented as 0, which would turn
        // every refusal into "lowering needs privileges".
        assert_eq!(
            outcome_from_errno(EPERM, None, 5),
            ReniceOutcome::NotPermitted {
                attempted: None,
                to: 5
            }
        );
    }

    #[test]
    fn a_refused_raise_is_explained_by_ownership_and_a_refused_drop_by_privilege() {
        let lowering = ReniceOutcome::NotPermitted {
            attempted: Some(NiceDirection::MoreDemanding),
            to: -5,
        }
        .message();
        assert!(lowering.contains("lowering"), "{lowering}");
        let raising = ReniceOutcome::NotPermitted {
            attempted: Some(NiceDirection::Nicer),
            to: 5,
        }
        .message();
        assert!(raising.contains("another user"), "{raising}");
    }

    #[test]
    fn a_successful_write_reports_what_the_kernel_says_not_what_was_asked() {
        let agreed = ReniceOutcome::Applied {
            from: Some(0),
            to: 5,
            confirmed: Some(5),
        };
        assert!(agreed.is_applied());
        assert!(agreed.is_expected());
        let message = agreed.message();
        assert!(message.contains('0'), "{message}");
        assert!(message.contains('5'), "{message}");

        let disagreed = ReniceOutcome::Applied {
            from: Some(0),
            to: 5,
            confirmed: Some(3),
        }
        .message();
        assert!(disagreed.contains("kernel reports 3"), "{disagreed}");

        let unread = ReniceOutcome::Applied {
            from: None,
            to: 7,
            confirmed: None,
        }
        .message();
        assert!(unread.contains('7'), "{unread}");
    }

    #[test]
    fn a_reused_pid_is_the_loudest_outcome_and_names_both_start_keys() {
        let requested = ProcessIdentity::new(FIXTURE_PID, FIXTURE_START_KEY);
        let outcome = ReniceOutcome::Reused {
            requested,
            found: Some(ProcessIdentity::new(FIXTURE_PID, 99)),
        };
        let message = outcome.message();
        assert!(message.contains("aborted"), "{message}");
        assert!(message.contains("4242"), "{message}");
        assert!(message.contains("88213700"), "{message}");
        assert!(message.contains("99"), "{message}");
        assert!(message.contains("nothing was changed"), "{message}");
        assert!(
            !outcome.is_expected(),
            "coming one step from renicing the wrong process is worth telling the user"
        );

        // macOS cannot name what the PID became; the refusal still has to be clear.
        let opaque = ReniceOutcome::Reused {
            requested,
            found: None,
        }
        .message();
        assert!(opaque.contains("no longer refers"), "{opaque}");
        assert!(opaque.contains("nothing was changed"), "{opaque}");
    }

    #[test]
    fn every_outcome_explains_itself_and_only_two_are_routine() {
        let outcomes = [
            ReniceOutcome::Applied {
                from: Some(0),
                to: 5,
                confirmed: Some(5),
            },
            ReniceOutcome::OutOfRange(OutOfRange { requested: 40 }),
            ReniceOutcome::NotASingleProcess,
            ReniceOutcome::Vanished,
            ReniceOutcome::Reused {
                requested: ProcessIdentity::new(1, 2),
                found: None,
            },
            ReniceOutcome::AlreadyExited,
            ReniceOutcome::Unverifiable {
                reason: "permission denied",
            },
            ReniceOutcome::NotPermitted {
                attempted: None,
                to: 5,
            },
            ReniceOutcome::Failed { errno: 5 },
            ReniceOutcome::Unsupported,
        ];
        for outcome in outcomes {
            let message = outcome.message();
            assert!(!message.is_empty());
            assert!(message.is_ascii(), "{message}");
        }
        let routine = outcomes.iter().filter(|o| o.is_expected()).count();
        assert_eq!(routine, 2, "only Applied and Vanished are routine (§14.1)");
        assert_eq!(outcomes.iter().filter(|o| o.is_applied()).count(), 1);
    }

    #[test]
    fn an_unverifiable_identity_says_why_and_changes_nothing() {
        let message = ReniceOutcome::Unverifiable {
            reason: "permission denied",
        }
        .message();
        assert!(message.contains("could not confirm"), "{message}");
        assert!(message.contains("permission denied"), "{message}");
        assert!(message.contains("nothing was changed"), "{message}");
    }

    #[test]
    fn the_capability_is_reported_as_available_exactly_where_a_write_is_possible() {
        assert_eq!(
            capability_state(),
            if SUPPORTED {
                CapabilityState::Available
            } else {
                CapabilityState::Unsupported
            }
        );
        // Never PermissionDenied: that would put a "privileges would help" hint on
        // the whole capability, when in fact raising our own processes' niceness
        // always works.
        assert!(!capability_state().privileges_might_help());
        assert_eq!(
            SUPPORTED,
            cfg!(any(
                target_os = "linux",
                all(target_os = "macos", feature = "macos-native")
            ))
        );
    }

    /// A process row with nothing measured, which is the state §4 requires a
    /// fixture to start from: a test that cares about a value has to say so.
    fn process_row() -> ProcessSnapshot {
        use monitrs_core::model::{ProcessIo, ProcessMemory};

        ProcessSnapshot {
            identity: ProcessIdentity::new(7, 11),
            parent_pid: None,
            name: "sh".into(),
            command: "sh".into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::WarmingUp,
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    #[test]
    fn a_target_built_from_the_model_carries_unknowns_as_unknown() {
        use monitrs_core::model::UserIdentity;

        let mut process = process_row();
        process.user = MetricState::Available(UserIdentity {
            uid: 501,
            name: Some("someone".into()),
        });

        let measured = ReniceTarget::from_snapshot(&process, &MetricState::Available(3));
        assert_eq!(measured.identity, ProcessIdentity::new(7, 11));
        assert_eq!(measured.state, ProcessState::Running);
        assert_eq!(measured.current_nice, Some(3));
        assert_eq!(measured.owner_uid, Some(501));

        // A niceness that was never measured must not become 0.
        let warming = ReniceTarget::from_snapshot(&process, &MetricState::WarmingUp);
        assert_eq!(warming.current_nice, None);
        let denied = ReniceTarget::from_snapshot(&process, &MetricState::PermissionDenied);
        assert_eq!(denied.current_nice, None);

        // A stale value is still the best basis for an advisory forecast.
        let stale = ReniceTarget::from_snapshot(
            &process,
            &MetricState::Stale {
                value: -4,
                age: core::time::Duration::from_secs(2),
            },
        );
        assert_eq!(stale.current_nice, Some(-4));

        // A denied owner is unknown, not root.
        process.user = MetricState::PermissionDenied;
        let unowned = ReniceTarget::from_snapshot(&process, &MetricState::Available(0));
        assert_eq!(unowned.owner_uid, None);

        let minimal = ReniceTarget::new(ProcessIdentity::new(7, 11), ProcessState::Sleeping);
        assert_eq!(minimal.current_nice, None);
        assert_eq!(minimal.owner_uid, None);
    }

    #[test]
    fn a_nice_value_that_does_not_fit_an_i8_is_refused_rather_than_wrapped() {
        // 256 truncates to 0 and -236 to 20; both must be refusals, not writes.
        for value in [256, -256, 276, 20, -21] {
            assert!(Nice::new(value).is_err(), "{value} must be refused");
        }
    }

    /// The Linux revalidation mapping, exercised from the checked-in `/proc`
    /// fixtures. These run on any host, which is the point of keeping the Linux
    /// parsers ungated (§17.2).
    mod linux_revalidation {
        use super::*;
        use crate::linux::fixtures;
        use crate::linux::revalidate;

        fn identity() -> ProcessIdentity {
            ProcessIdentity::new(FIXTURE_PID, FIXTURE_START_KEY)
        }

        #[test]
        fn a_matching_identity_is_the_only_confirmation_that_permits_a_write() {
            let decision = revalidate(identity(), INERT_SIGNAL, Ok(fixtures::PID_STAT_SIMPLE));
            assert_eq!(confirmation_from(decision), Confirmation::Current);
        }

        #[test]
        fn a_pid_reused_within_the_same_second_is_refused_with_both_keys() {
            // The case a whole-second start time cannot see, and the reason renice
            // reuses the signal path's revalidation instead of comparing PIDs.
            let decision = revalidate(
                identity(),
                INERT_SIGNAL,
                Ok(fixtures::PID_STAT_REUSED_SAME_SECOND),
            );
            match confirmation_from(decision) {
                Confirmation::Changed { found: Some(found) } => {
                    assert_eq!(found.pid, FIXTURE_PID);
                    assert_ne!(found.start_key, FIXTURE_START_KEY);
                }
                other => panic!("a recycled PID must not be renicable, got {other:?}"),
            }
        }

        #[test]
        fn a_vanished_process_and_a_zombie_are_distinguished() {
            use crate::linux::ReadFailure;

            let vanished = revalidate(identity(), INERT_SIGNAL, Err(ReadFailure::Missing));
            assert_eq!(confirmation_from(vanished), Confirmation::Vanished);

            let zombie = revalidate(
                ProcessIdentity::new(7_331, 88_213_000),
                INERT_SIGNAL,
                Ok(fixtures::PID_STAT_ZOMBIE),
            );
            assert_eq!(confirmation_from(zombie), Confirmation::AlreadyExited);
        }

        #[test]
        fn an_unreadable_or_truncated_stat_refuses_and_carries_the_reason() {
            use crate::linux::ReadFailure;

            for failure in [
                ReadFailure::Denied,
                ReadFailure::Oversized,
                ReadFailure::Failed,
            ] {
                let decision = revalidate(identity(), INERT_SIGNAL, Err(failure));
                assert_eq!(
                    confirmation_from(decision),
                    Confirmation::Unverifiable {
                        reason: failure.describe()
                    }
                );
            }
            let truncated = revalidate(identity(), INERT_SIGNAL, Ok(fixtures::PID_STAT_TRUNCATED));
            assert!(matches!(
                confirmation_from(truncated),
                Confirmation::Unverifiable { .. }
            ));
        }

        #[test]
        fn a_process_name_full_of_parentheses_does_not_defeat_the_check() {
            let decision = revalidate(
                ProcessIdentity::new(9_182, 88_100_000),
                INERT_SIGNAL,
                Ok(fixtures::PID_STAT_WEIRD_NAME),
            );
            assert_eq!(confirmation_from(decision), Confirmation::Current);
        }
    }

    /// Errno numbering, checked against the platform's own headers.
    ///
    /// Only possible where `libc` is linked, which is macOS with `macos-native`. One
    /// target is enough: the point is that the numbers are not invented, and they are
    /// the same three on both platforms.
    #[cfg(all(target_os = "macos", feature = "macos-native"))]
    #[test]
    fn the_errno_constants_are_the_platforms_own() {
        assert_eq!(EPERM, libc::EPERM);
        assert_eq!(ESRCH, libc::ESRCH);
        assert_eq!(EACCES, libc::EACCES);
        assert_eq!(PRIO_PROCESS, libc::PRIO_PROCESS);
    }

    /// Live tests. Every one of them acts on **this process only**: §15.1 and plain
    /// courtesy forbid a test suite from changing the priority of a process it does
    /// not own, and a test that needs privileges is skipped rather than escalated.
    ///
    /// `#[ignore]`d because they read and write live kernel state; CI runs them with
    /// `-- --ignored --test-threads=1`.
    mod live {
        use super::*;

        /// Our own identity, read exactly the way this platform's revalidation reads
        /// it — from `/proc/self/stat` through the same parser, so the start key is
        /// the clock-tick one `revalidate` compares against.
        #[cfg(target_os = "linux")]
        fn own_identity() -> ProcessIdentity {
            use crate::linux::ProcRoot;
            use crate::linux::process::parse_pid_stat;

            let pid = std::process::id();
            let bytes = ProcRoot::live()
                .read_pid(pid, "stat")
                .expect("our own /proc/<pid>/stat is readable");
            parse_pid_stat(&bytes)
                .expect("our own stat parses")
                .identity()
        }

        /// Our own identity, from the kernel's process table.
        ///
        /// Taken from `MacosCollector` rather than from `sysinfo` because
        /// `identity_is_current` compares microsecond start keys, and a whole-second
        /// key from the baseline would not match its own revalidation.
        #[cfg(all(target_os = "macos", feature = "macos-native"))]
        fn own_identity() -> ProcessIdentity {
            use crate::macos::MacosCollector;
            use crate::source::{SampleTick, SnapshotSource};

            let mut collector = MacosCollector::new().expect("the macOS collector starts");
            let tick = SampleTick::first(std::time::Instant::now(), std::time::SystemTime::now());
            collector.sample(&tick).expect("one sample succeeds");
            let me = std::process::id();
            collector
                .kernel_table()
                .iter()
                .find(|process| process.identity.pid == me)
                .map(|process| process.identity)
                .expect("this process is in the kernel's process table")
        }

        /// A target describing this process, at its real current niceness.
        ///
        /// The `None` arm below is the *only* legitimate way for a live test to skip,
        /// and it exists only on a build with no platform layer at all. Everywhere
        /// else this panics rather than returning `None`, because a live test that
        /// silently skipped on the platform it was written for would be testing
        /// nothing while reporting success.
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "macos-native")
        ))]
        fn own_target() -> Option<(ReniceTarget, i8)> {
            let identity = own_identity();
            let current = live_nice(identity.pid).expect("our own nice value is readable");
            Some((
                ReniceTarget {
                    identity,
                    state: ProcessState::Running,
                    current_nice: Some(current),
                    // Our own process, so the owner is our own effective uid.
                    owner_uid: Privileges::current().euid(),
                },
                current,
            ))
        }

        /// No platform layer, so there is nothing to smoke-test (see above).
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "macos-native")
        )))]
        fn own_target() -> Option<(ReniceTarget, i8)> {
            None
        }

        #[test]
        #[ignore = "platform smoke test: reads live kernel state"]
        fn a_failed_getpriority_is_unknown_rather_than_a_nice_value_of_minus_one() {
            // The classic `getpriority` trap, live: -1 is a legitimate nice value, so a
            // failure can only be told apart from it through `errno`. If the clear-then-
            // check protocol were wrong, a PID that cannot exist would read as "nice -1"
            // and the forecast would then reason about a value that does not exist.
            if !cfg!(any(target_os = "linux", target_os = "macos")) {
                return;
            }
            assert_eq!(live_nice(0x7fff_0000), None);
            assert!(
                live_nice(std::process::id()).is_some(),
                "our own priority is readable"
            );
        }

        #[test]
        #[ignore = "platform smoke test: reads and writes this process's own nice value"]
        fn renicing_ourselves_to_a_higher_value_takes_effect() {
            let Some((target, before)) = own_target() else {
                return;
            };
            if before >= MAX_NICE {
                // Nothing higher to move to; nothing to prove either.
                return;
            }
            let requested = before.saturating_add(1);
            let outcome = renice(&target, i32::from(requested));
            assert_eq!(
                outcome,
                ReniceOutcome::Applied {
                    from: Some(before),
                    to: requested,
                    confirmed: Some(requested)
                },
                "{}",
                outcome.message()
            );
            // The independent check: ask the kernel again, through the same call the
            // dry run uses.
            assert_eq!(live_nice(target.identity.pid), Some(requested));
        }

        #[test]
        #[ignore = "platform smoke test: reads and writes this process's own nice value"]
        fn a_stale_start_key_on_our_own_pid_is_refused_and_changes_nothing() {
            let Some((target, before)) = own_target() else {
                return;
            };
            let stale = ReniceTarget {
                identity: ProcessIdentity::new(
                    target.identity.pid,
                    target.identity.start_key.wrapping_add(1),
                ),
                ..target
            };
            let outcome = renice(&stale, i32::from(MAX_NICE));
            assert!(
                matches!(outcome, ReniceOutcome::Reused { .. }),
                "a stale identity must abort, got {outcome:?}"
            );
            assert!(!outcome.is_applied());
            assert_eq!(
                live_nice(target.identity.pid),
                Some(before),
                "the refused write must not have happened"
            );
        }

        #[test]
        #[ignore = "platform smoke test: reads live kernel state"]
        fn the_dry_run_agrees_that_raising_our_own_niceness_is_permitted() {
            let Some((target, before)) = own_target() else {
                return;
            };
            if before >= MAX_NICE {
                return;
            }
            let forecast = dry_run(&target, i32::from(MAX_NICE));
            assert!(
                forecast.is_permitted(),
                "raising our own niceness must be permitted, got {forecast:?}"
            );
            // And it must not have written anything.
            assert_eq!(live_nice(target.identity.pid), Some(before));
        }

        #[test]
        #[ignore = "platform smoke test: reads live kernel state"]
        fn the_dry_run_refuses_a_value_outside_the_range_without_touching_the_kernel() {
            let Some((target, before)) = own_target() else {
                return;
            };
            assert_eq!(
                dry_run(&target, 42),
                ReniceForecast::OutOfRange(OutOfRange { requested: 42 })
            );
            assert_eq!(live_nice(target.identity.pid), Some(before));
        }

        #[test]
        #[ignore = "platform smoke test: attempts a write that needs privileges"]
        fn lowering_our_own_niceness_reports_the_refusal_rather_than_failing_silently() {
            // Skipped as root, where the write would succeed and prove nothing about
            // the refusal path (§15.1: never escalate to make a test pass).
            if Privileges::current().is_root() {
                return;
            }
            let Some((target, before)) = own_target() else {
                return;
            };
            if before <= MIN_NICE {
                return;
            }
            let requested = before.saturating_sub(1);
            let outcome = renice(&target, i32::from(requested));
            match outcome {
                // The usual answer: RLIMIT_NICE leaves no headroom by default.
                ReniceOutcome::NotPermitted { attempted, to } => {
                    assert_eq!(attempted, Some(NiceDirection::MoreDemanding));
                    assert_eq!(to, requested);
                    assert!(outcome.message().contains("privileges"));
                    assert_eq!(live_nice(target.identity.pid), Some(before));
                }
                // A raised RLIMIT_NICE or CAP_SYS_NICE genuinely permits it, which is
                // exactly why the forecast never vetoes the attempt.
                ReniceOutcome::Applied { confirmed, .. } => {
                    assert_eq!(confirmed, Some(requested));
                }
                other => panic!("expected a clear refusal or a real write, got {other:?}"),
            }
        }

        #[test]
        #[ignore = "platform smoke test: reads the live process table"]
        fn a_pid_that_cannot_exist_is_never_reported_as_applied() {
            let phantom =
                ReniceTarget::new(ProcessIdentity::new(0x7fff_0000, 1), ProcessState::Sleeping);
            let outcome = renice(&phantom, 10);
            assert!(!outcome.is_applied(), "got {outcome:?}");
            assert!(
                matches!(
                    outcome,
                    ReniceOutcome::Vanished
                        | ReniceOutcome::Reused { .. }
                        | ReniceOutcome::Unverifiable { .. }
                        | ReniceOutcome::Unsupported
                ),
                "got {outcome:?}"
            );
        }
    }
}
