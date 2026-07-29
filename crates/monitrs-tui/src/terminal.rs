//! Terminal lifecycle: an RAII guard that restores every mode it changed (§14.3).
//!
//! # Why this is not `ratatui::init()`
//!
//! Ratatui's own `init`/`restore` helpers cover raw mode and the alternate
//! screen only, and they restore unconditionally. §14.3 additionally requires
//! optional mouse capture, cursor hiding, **partial-initialization safety**, and
//! **idempotent** restoration. Restoring a mode that was never entered is not
//! harmless: a second `LeaveAlternateScreen` restores the saved cursor position
//! and can overwrite a panic message that was just printed on the normal screen.
//! So this module tracks exactly which steps were applied and undoes exactly
//! those, exactly once.
//!
//! # Testability
//!
//! §14.3 requires the restoration logic to be testable without an interactive
//! terminal, so every side effect goes through [`TerminalControl`].
//! [`CrosstermControl`] is the real implementation; the unit tests use a
//! recording fake and assert the operation *order*.
//!
//! # Logging (§14.2)
//!
//! While the alternate screen is active, anything written to stdout or stderr
//! lands on top of the rendered frame. This module never prints: failures are
//! reported through [`TerminalError`] or the `tracing` macros, and the binary is
//! responsible for pointing `tracing` at a file (or nowhere). Code that is
//! tempted to print can ask [`StepRegistry::alternate_screen_active`] whether
//! stdout is currently owned by the UI.

use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};
use std::io::{self, Stdout, Write, stdout};
use std::sync::Once;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::{DefaultTerminal, Terminal};

/// One reversible terminal modification (§14.3).
///
/// Each variant is a single mode that can be applied and undone independently,
/// which is what makes partial initialization recoverable.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TerminalStep {
    /// Raw mode: no line buffering, no echo, keys delivered as typed.
    RawMode,
    /// The alternate screen buffer, so the user's scrollback survives.
    AlternateScreen,
    /// Mouse event reporting. Optional, off by default (`display.mouse` in §12).
    MouseCapture,
    /// A hidden cursor, because the UI draws its own selection indicator.
    HiddenCursor,
}

impl TerminalStep {
    /// The order steps are applied in.
    ///
    /// Raw mode first because it has the widest effect and is the most important
    /// to undo; the cursor is hidden last so it is hidden *on* the alternate
    /// screen. Restoration walks this array in reverse (§14.3).
    pub const SETUP_ORDER: [Self; 4] = [
        Self::RawMode,
        Self::AlternateScreen,
        Self::MouseCapture,
        Self::HiddenCursor,
    ];

    /// Human-readable name used in error messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RawMode => "raw mode",
            Self::AlternateScreen => "alternate screen",
            Self::MouseCapture => "mouse capture",
            Self::HiddenCursor => "hidden cursor",
        }
    }

    /// The bit this step occupies in a [`StepSet`].
    const fn bit(self) -> u8 {
        match self {
            Self::RawMode => 1,
            Self::AlternateScreen => 1 << 1,
            Self::MouseCapture => 1 << 2,
            Self::HiddenCursor => 1 << 3,
        }
    }
}

impl fmt::Display for TerminalStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A set of [`TerminalStep`]s.
///
/// A set rather than a list because the undo order is fixed (the reverse of
/// [`TerminalStep::SETUP_ORDER`]), and because it fits in one atomic byte, which
/// is what lets the panic hook read it without a lock.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StepSet(u8);

impl StepSet {
    /// No steps.
    pub const EMPTY: Self = Self(0);

    /// Every step.
    pub const ALL: Self = Self(0b1111);

    /// Whether `step` is a member.
    #[must_use]
    pub const fn contains(self, step: TerminalStep) -> bool {
        self.0 & step.bit() != 0
    }

    /// This set plus `step`.
    #[must_use]
    pub const fn with(self, step: TerminalStep) -> Self {
        Self(self.0 | step.bit())
    }

    /// This set minus `step`.
    #[must_use]
    pub const fn without(self, step: TerminalStep) -> Self {
        Self(self.0 & !step.bit())
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many steps are in the set.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// Members in application order.
    pub fn iter_setup_order(self) -> impl Iterator<Item = TerminalStep> {
        TerminalStep::SETUP_ORDER
            .into_iter()
            .filter(move |step| self.contains(*step))
    }

    /// Members in restoration order, i.e. the reverse of application order.
    pub fn iter_restore_order(self) -> impl Iterator<Item = TerminalStep> {
        TerminalStep::SETUP_ORDER
            .into_iter()
            .rev()
            .filter(move |step| self.contains(*step))
    }
}

/// Which modes a [`TerminalGuard`] should enter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TerminalSettings {
    /// Enable raw mode. Required for key-by-key input; only a test would clear it.
    pub raw_mode: bool,
    /// Enter the alternate screen so the user's scrollback is untouched.
    pub alternate_screen: bool,
    /// Enable mouse reporting. Off by default (§12 `display.mouse = false`).
    pub mouse_capture: bool,
    /// Hide the hardware cursor; the UI draws its own selection indicator.
    pub hide_cursor: bool,
}

impl TerminalSettings {
    /// The steps these settings ask for.
    #[must_use]
    pub const fn steps(self) -> StepSet {
        let mut set = StepSet::EMPTY;
        if self.raw_mode {
            set = set.with(TerminalStep::RawMode);
        }
        if self.alternate_screen {
            set = set.with(TerminalStep::AlternateScreen);
        }
        if self.mouse_capture {
            set = set.with(TerminalStep::MouseCapture);
        }
        if self.hide_cursor {
            set = set.with(TerminalStep::HiddenCursor);
        }
        set
    }

    /// These settings with mouse capture set to `enabled`.
    #[must_use]
    pub const fn with_mouse_capture(mut self, enabled: bool) -> Self {
        self.mouse_capture = enabled;
        self
    }
}

impl Default for TerminalSettings {
    /// Raw mode, alternate screen and a hidden cursor; no mouse capture.
    fn default() -> Self {
        Self {
            raw_mode: true,
            alternate_screen: true,
            mouse_capture: false,
            hide_cursor: true,
        }
    }
}

/// A terminal error, separated from every other failure class by §14.1.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// A setup step failed. The steps that already succeeded have been undone.
    #[error("could not enter {step}: {source}")]
    Setup {
        /// The step that failed.
        step: TerminalStep,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// A restoration step failed. Restoration continued regardless.
    #[error("could not leave {step}: {source}")]
    Restore {
        /// The step that failed.
        step: TerminalStep,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The Ratatui backend could not be created, usually because the terminal
    /// size could not be queried.
    #[error("could not create the terminal backend: {source}")]
    Backend {
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// The side effects a [`TerminalGuard`] performs, abstracted so that §14.3's
/// restoration rules can be unit-tested without an interactive terminal.
///
/// Implementations must be idempotent per operation where the terminal allows
/// it, and must never panic: a panic here happens with the screen in an unknown
/// state.
pub trait TerminalControl {
    /// Enter one mode.
    fn apply(&mut self, step: TerminalStep) -> io::Result<()>;

    /// Leave one mode.
    fn undo(&mut self, step: TerminalStep) -> io::Result<()>;
}

/// The real [`TerminalControl`], driving Crossterm.
#[derive(Debug)]
pub struct CrosstermControl<W: Write> {
    writer: W,
}

impl<W: Write> CrosstermControl<W> {
    /// Wraps an arbitrary writer, which is what the integration tests use.
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl CrosstermControl<Stdout> {
    /// The production control, writing escape sequences to stdout.
    #[must_use]
    pub fn stdout() -> Self {
        Self::new(stdout())
    }
}

impl<W: Write> TerminalControl for CrosstermControl<W> {
    fn apply(&mut self, step: TerminalStep) -> io::Result<()> {
        match step {
            // Raw mode is a tty attribute, not an escape sequence, so it does not
            // go through the writer.
            TerminalStep::RawMode => enable_raw_mode(),
            TerminalStep::AlternateScreen => execute!(self.writer, EnterAlternateScreen),
            TerminalStep::MouseCapture => execute!(self.writer, EnableMouseCapture),
            TerminalStep::HiddenCursor => execute!(self.writer, cursor::Hide),
        }
    }

    fn undo(&mut self, step: TerminalStep) -> io::Result<()> {
        match step {
            TerminalStep::RawMode => disable_raw_mode(),
            TerminalStep::AlternateScreen => execute!(self.writer, LeaveAlternateScreen),
            TerminalStep::MouseCapture => execute!(self.writer, DisableMouseCapture),
            TerminalStep::HiddenCursor => execute!(self.writer, cursor::Show),
        }
    }
}

/// Which terminal modes are currently applied, readable from a panic hook.
///
/// The panic hook cannot borrow the guard — it may run on any thread, and during
/// a panic the guard is mid-unwind — so the applied set is mirrored into one
/// atomic byte. Taking from the registry is a claim: whichever of the guard and
/// the panic hook gets there first performs the restoration, and the other does
/// nothing. That is what makes restoration exactly-once rather than merely
/// idempotent (§14.3).
#[derive(Debug, Default)]
pub struct StepRegistry {
    bits: AtomicU8,
}

impl StepRegistry {
    /// An empty registry. `const` so tests can declare their own isolated one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bits: AtomicU8::new(0),
        }
    }

    /// Records `step` as applied.
    fn add(&self, step: TerminalStep) {
        self.bits.fetch_or(step.bit(), Ordering::SeqCst);
    }

    /// Atomically clears `wanted` and returns the subset that was still set,
    /// i.e. the steps the caller is now responsible for undoing.
    fn take(&self, wanted: StepSet) -> StepSet {
        let previous = self.bits.fetch_and(!wanted.0, Ordering::SeqCst);
        StepSet(previous & wanted.0)
    }

    /// The steps currently applied.
    #[must_use]
    pub fn snapshot(&self) -> StepSet {
        StepSet(self.bits.load(Ordering::SeqCst))
    }

    /// Whether the alternate screen is active, i.e. whether writing to stdout or
    /// stderr would corrupt the display (§14.2).
    #[must_use]
    pub fn alternate_screen_active(&self) -> bool {
        self.snapshot().contains(TerminalStep::AlternateScreen)
    }
}

/// The process-wide registry the panic hook reads.
pub static TERMINAL_STEPS: StepRegistry = StepRegistry::new();

/// Whether writing to stdout or stderr right now would land on top of a rendered
/// frame (§14.2). Log sinks and diagnostic prints must check this.
#[must_use]
pub fn alternate_screen_active() -> bool {
    TERMINAL_STEPS.alternate_screen_active()
}

/// An RAII terminal guard (§14.3).
///
/// Entering modes and leaving them is symmetric and tracked, so a failure
/// half-way through setup leaves the terminal exactly as it was found, and
/// dropping the guard restores it once — even if [`restore`](Self::restore) was
/// already called explicitly, or the panic hook got there first.
#[derive(Debug)]
#[must_use = "the terminal is only restored when the guard is dropped"]
pub struct TerminalGuard<C: TerminalControl> {
    control: C,
    applied: StepSet,
    registry: &'static StepRegistry,
    restored: bool,
}

impl<C: TerminalControl> TerminalGuard<C> {
    /// Enters the requested modes, recording them in [`TERMINAL_STEPS`].
    ///
    /// # Errors
    ///
    /// Returns the first failing step. Every step that had already succeeded is
    /// undone before returning, so a partially initialized terminal is never
    /// handed back to the user (§14.3). The failing step itself is *not* undone:
    /// it did not report success, so its state is unknown and a speculative undo
    /// could make things worse.
    pub fn install(control: C, settings: TerminalSettings) -> Result<Self, TerminalError> {
        Self::install_with_registry(control, settings, &TERMINAL_STEPS)
    }

    /// [`install`](Self::install) against an explicit registry.
    ///
    /// Production code wants the process-wide [`TERMINAL_STEPS`]; tests declare
    /// their own so that parallel test threads cannot observe each other's steps.
    ///
    /// # Errors
    ///
    /// As [`install`](Self::install).
    pub fn install_with_registry(
        control: C,
        settings: TerminalSettings,
        registry: &'static StepRegistry,
    ) -> Result<Self, TerminalError> {
        let mut guard = Self {
            control,
            applied: StepSet::EMPTY,
            registry,
            restored: false,
        };

        for step in settings.steps().iter_setup_order() {
            match guard.control.apply(step) {
                Ok(()) => {
                    guard.applied = guard.applied.with(step);
                    registry.add(step);
                }
                // Returning here drops `guard`, and its `Drop` undoes the prefix
                // that did succeed. That is the partial-initialization contract.
                Err(source) => return Err(TerminalError::Setup { step, source }),
            }
        }

        Ok(guard)
    }

    /// The steps this guard is still responsible for undoing.
    #[must_use]
    pub const fn applied_steps(&self) -> StepSet {
        self.applied
    }

    /// Whether restoration has already happened.
    #[must_use]
    pub const fn is_restored(&self) -> bool {
        self.restored
    }

    /// Borrows the underlying control, e.g. to write directly to the terminal.
    pub const fn control_mut(&mut self) -> &mut C {
        &mut self.control
    }

    /// Undoes every applied step, in reverse order. Idempotent.
    ///
    /// Restoration does not stop at the first failure: leaving raw mode matters
    /// even if leaving the alternate screen failed. The first error is returned,
    /// later ones are logged.
    ///
    /// # Errors
    ///
    /// The first [`TerminalError::Restore`] encountered.
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;

        // Claim the steps: if the panic hook already restored them, this is empty
        // and we must not undo anything a second time.
        let owed = self.registry.take(self.applied);
        self.applied = StepSet::EMPTY;
        restore_steps(&mut self.control, owed)
    }
}

impl<C: TerminalControl> Drop for TerminalGuard<C> {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            // §14.2: never print while the screen may still be ours. `tracing`
            // goes to the configured file sink, or nowhere.
            tracing::error!(%error, "terminal restoration failed");
        }
    }
}

/// Undoes `steps` in restoration order, attempting all of them.
fn restore_steps<C: TerminalControl>(control: &mut C, steps: StepSet) -> Result<(), TerminalError> {
    let mut first_error: Option<TerminalError> = None;

    for step in steps.iter_restore_order() {
        if let Err(source) = control.undo(step) {
            if first_error.is_none() {
                first_error = Some(TerminalError::Restore { step, source });
            } else {
                tracing::warn!(%step, %source, "further terminal restoration failure");
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Undoes whatever `registry` still records as applied.
///
/// This is the body of the panic hook, factored out so it can be tested with a
/// fake control and an isolated registry.
fn restore_registered<C: TerminalControl>(
    control: &mut C,
    registry: &StepRegistry,
) -> Result<(), TerminalError> {
    restore_steps(control, registry.take(StepSet::ALL))
}

/// Restores the terminal immediately, from anywhere, at most once.
///
/// Safe to call when nothing was ever applied: it then does nothing.
pub fn emergency_restore() {
    let mut control = CrosstermControl::stdout();
    if let Err(error) = restore_registered(&mut control, &TERMINAL_STEPS) {
        tracing::error!(%error, "emergency terminal restoration failed");
    }
}

/// Installs a panic hook that restores the terminal *before* the panic message
/// is printed (§14.3).
///
/// Ordering is the whole point: the message must land on the normal screen, in
/// cooked mode, where the user can read and copy it. This relies on the release
/// profile keeping `panic = "unwind"` (§19.4) — with `panic = "abort"` the hook
/// still runs, but no `Drop` does, so the hook is the only chance.
///
/// Idempotent: calling it twice does not chain two hooks. Call it before
/// installing the guard so a failure inside [`TerminalGuard::install`] is
/// covered too.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();
            previous(info);
        }));
    });
}

/// Creates the Ratatui terminal that draws to stdout.
///
/// Separate from [`TerminalGuard`] because the guard owns *terminal modes* while
/// this owns the *frame buffer*: the guard must outlive the terminal so the
/// screen is restored after the last draw.
///
/// # Errors
///
/// [`TerminalError::Backend`] if the terminal size cannot be queried, which is
/// what happens when stdout is not a tty.
pub fn create_terminal() -> Result<DefaultTerminal, TerminalError> {
    Terminal::new(CrosstermBackend::new(stdout()))
        .map_err(|source| TerminalError::Backend { source })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;

    use super::*;

    /// One recorded terminal operation.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Op {
        Apply(TerminalStep),
        Undo(TerminalStep),
    }

    /// A [`TerminalControl`] that records instead of touching a terminal, and can
    /// be told to fail a specific step.
    #[derive(Debug)]
    struct FakeControl {
        log: Rc<RefCell<Vec<Op>>>,
        fail_apply: Option<TerminalStep>,
        fail_undo: Option<TerminalStep>,
    }

    impl FakeControl {
        fn new() -> (Self, Rc<RefCell<Vec<Op>>>) {
            let log = Rc::new(RefCell::new(Vec::new()));
            let control = Self {
                log: Rc::clone(&log),
                fail_apply: None,
                fail_undo: None,
            };
            (control, log)
        }

        fn failing_apply(step: TerminalStep) -> (Self, Rc<RefCell<Vec<Op>>>) {
            let (mut control, log) = Self::new();
            control.fail_apply = Some(step);
            (control, log)
        }

        fn failing_undo(step: TerminalStep) -> (Self, Rc<RefCell<Vec<Op>>>) {
            let (mut control, log) = Self::new();
            control.fail_undo = Some(step);
            (control, log)
        }
    }

    impl TerminalControl for FakeControl {
        fn apply(&mut self, step: TerminalStep) -> io::Result<()> {
            if self.fail_apply == Some(step) {
                return Err(io::Error::other("fake apply failure"));
            }
            self.log.borrow_mut().push(Op::Apply(step));
            Ok(())
        }

        fn undo(&mut self, step: TerminalStep) -> io::Result<()> {
            if self.fail_undo == Some(step) {
                self.log.borrow_mut().push(Op::Undo(step));
                return Err(io::Error::other("fake undo failure"));
            }
            self.log.borrow_mut().push(Op::Undo(step));
            Ok(())
        }
    }

    fn all_steps() -> TerminalSettings {
        TerminalSettings::default().with_mouse_capture(true)
    }

    #[test]
    fn setup_enters_modes_in_documented_order() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::new();

        let guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed with a fake control");

        assert_eq!(
            *log.borrow(),
            vec![
                Op::Apply(TerminalStep::RawMode),
                Op::Apply(TerminalStep::AlternateScreen),
                Op::Apply(TerminalStep::MouseCapture),
                Op::Apply(TerminalStep::HiddenCursor),
            ]
        );
        assert_eq!(guard.applied_steps(), StepSet::ALL);
        drop(guard);
    }

    #[test]
    fn restore_undoes_modes_in_reverse_order() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::new();
        let mut guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed");
        log.borrow_mut().clear();

        guard.restore().expect("restore should succeed");

        assert_eq!(
            *log.borrow(),
            vec![
                Op::Undo(TerminalStep::HiddenCursor),
                Op::Undo(TerminalStep::MouseCapture),
                Op::Undo(TerminalStep::AlternateScreen),
                Op::Undo(TerminalStep::RawMode),
            ]
        );
    }

    #[test]
    fn restoring_twice_undoes_each_mode_once() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::new();
        let mut guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed");
        log.borrow_mut().clear();

        guard.restore().expect("first restore should succeed");
        guard.restore().expect("second restore should be a no-op");
        drop(guard);

        let undos = log.borrow().len();
        assert_eq!(
            undos, 4,
            "expected exactly four undo operations, got {undos}"
        );
        assert!(REGISTRY.snapshot().is_empty());
    }

    #[test]
    fn dropping_after_explicit_restore_does_not_undo_again() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::new();
        let mut guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed");
        guard.restore().expect("restore should succeed");
        log.borrow_mut().clear();

        drop(guard);

        assert!(log.borrow().is_empty());
    }

    #[test]
    fn failed_setup_undoes_only_the_steps_that_succeeded() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::failing_apply(TerminalStep::MouseCapture);

        let error = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect_err("mouse capture was configured to fail");

        match error {
            TerminalError::Setup { step, .. } => assert_eq!(step, TerminalStep::MouseCapture),
            other => panic!("expected a setup error, got {other:?}"),
        }
        assert_eq!(
            *log.borrow(),
            vec![
                Op::Apply(TerminalStep::RawMode),
                Op::Apply(TerminalStep::AlternateScreen),
                // The cursor was never hidden and mouse capture never succeeded,
                // so neither is undone.
                Op::Undo(TerminalStep::AlternateScreen),
                Op::Undo(TerminalStep::RawMode),
            ]
        );
        assert!(
            REGISTRY.snapshot().is_empty(),
            "a failed setup must leave nothing registered for the panic hook"
        );
    }

    #[test]
    fn failed_first_step_applies_and_undoes_nothing() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::failing_apply(TerminalStep::RawMode);

        let error = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect_err("raw mode was configured to fail");

        assert!(matches!(
            error,
            TerminalError::Setup {
                step: TerminalStep::RawMode,
                ..
            }
        ));
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn restoration_continues_after_a_failing_step() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::failing_undo(TerminalStep::AlternateScreen);
        let mut guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed");
        log.borrow_mut().clear();

        let error = guard
            .restore()
            .expect_err("leaving the alternate screen was configured to fail");

        assert!(matches!(
            error,
            TerminalError::Restore {
                step: TerminalStep::AlternateScreen,
                ..
            }
        ));
        assert!(
            log.borrow().contains(&Op::Undo(TerminalStep::RawMode)),
            "raw mode must still be left when an earlier undo fails"
        );
    }

    #[test]
    fn only_requested_steps_are_applied() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::new();
        let settings = TerminalSettings {
            raw_mode: true,
            alternate_screen: false,
            mouse_capture: false,
            hide_cursor: false,
        };

        let guard = TerminalGuard::install_with_registry(control, settings, &REGISTRY)
            .expect("setup should succeed");

        assert_eq!(
            guard.applied_steps(),
            StepSet::EMPTY.with(TerminalStep::RawMode)
        );
        assert_eq!(*log.borrow(), vec![Op::Apply(TerminalStep::RawMode)]);
        drop(guard);
    }

    #[test]
    fn panic_while_the_guard_is_alive_restores_the_terminal() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, log) = FakeControl::new();

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
                .expect("setup should succeed");
            panic!("simulated render panic");
        }));

        assert!(outcome.is_err(), "the panic should propagate");
        let ops = log.borrow();
        assert_eq!(
            ops.iter().filter(|op| matches!(op, Op::Undo(_))).count(),
            4,
            "unwinding must undo all four modes: {ops:?}"
        );
        assert!(REGISTRY.snapshot().is_empty());
    }

    #[test]
    fn panic_hook_restoration_claims_the_steps_so_drop_does_nothing() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (control, guard_log) = FakeControl::new();
        let mut guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed");
        guard_log.borrow_mut().clear();

        // Stand in for the panic hook: a second control, restoring from the
        // registry alone, exactly as `emergency_restore` does.
        let (mut hook_control, hook_log) = FakeControl::new();
        restore_registered(&mut hook_control, &REGISTRY).expect("hook restore should succeed");

        assert_eq!(
            *hook_log.borrow(),
            vec![
                Op::Undo(TerminalStep::HiddenCursor),
                Op::Undo(TerminalStep::MouseCapture),
                Op::Undo(TerminalStep::AlternateScreen),
                Op::Undo(TerminalStep::RawMode),
            ]
        );

        guard.restore().expect("guard restore should be a no-op");
        drop(guard);
        assert!(
            guard_log.borrow().is_empty(),
            "the guard must not re-undo what the panic hook already undid"
        );
    }

    #[test]
    fn emergency_restore_on_an_untouched_registry_does_nothing() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        let (mut control, log) = FakeControl::new();

        restore_registered(&mut control, &REGISTRY).expect("nothing to restore");

        assert!(log.borrow().is_empty());
    }

    #[test]
    fn the_real_emergency_restore_is_a_no_op_when_no_guard_ever_ran() {
        // Exercises the production function and the process-wide registry. No
        // test installs a guard against `TERMINAL_STEPS`, so the claimed set is
        // empty and no escape sequence reaches the real terminal — which is
        // precisely the property a panic hook must have before setup happens.
        assert!(TERMINAL_STEPS.snapshot().is_empty());
        assert!(!alternate_screen_active());

        emergency_restore();

        assert!(TERMINAL_STEPS.snapshot().is_empty());
    }

    #[test]
    fn alternate_screen_activity_tracks_the_registry() {
        static REGISTRY: StepRegistry = StepRegistry::new();
        assert!(!REGISTRY.alternate_screen_active());

        let (control, _log) = FakeControl::new();
        let mut guard = TerminalGuard::install_with_registry(control, all_steps(), &REGISTRY)
            .expect("setup should succeed");
        assert!(
            REGISTRY.alternate_screen_active(),
            "§14.2 needs this to be true while the UI owns stdout"
        );

        guard.restore().expect("restore should succeed");
        assert!(!REGISTRY.alternate_screen_active());
    }

    #[test]
    fn step_set_iterates_setup_and_restore_orders() {
        let set = StepSet::EMPTY
            .with(TerminalStep::RawMode)
            .with(TerminalStep::HiddenCursor);

        assert_eq!(
            set.iter_setup_order().collect::<Vec<_>>(),
            vec![TerminalStep::RawMode, TerminalStep::HiddenCursor]
        );
        assert_eq!(
            set.iter_restore_order().collect::<Vec<_>>(),
            vec![TerminalStep::HiddenCursor, TerminalStep::RawMode]
        );
        assert_eq!(set.len(), 2);
        assert!(!set.contains(TerminalStep::MouseCapture));
        assert!(
            set.without(TerminalStep::RawMode)
                .contains(TerminalStep::HiddenCursor)
        );
    }

    #[test]
    fn registry_take_returns_only_what_was_set() {
        let registry = StepRegistry::new();
        registry.add(TerminalStep::RawMode);

        let taken = registry.take(StepSet::ALL);

        assert_eq!(taken, StepSet::EMPTY.with(TerminalStep::RawMode));
        assert!(registry.take(StepSet::ALL).is_empty(), "taking must clear");
    }

    #[test]
    fn default_settings_leave_mouse_capture_off() {
        let settings = TerminalSettings::default();

        assert!(!settings.steps().contains(TerminalStep::MouseCapture));
        assert!(settings.steps().contains(TerminalStep::RawMode));
        assert!(settings.steps().contains(TerminalStep::AlternateScreen));
        assert!(settings.steps().contains(TerminalStep::HiddenCursor));
        assert!(
            settings
                .with_mouse_capture(true)
                .steps()
                .contains(TerminalStep::MouseCapture)
        );
    }
}
