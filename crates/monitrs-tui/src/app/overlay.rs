//! The overlay stack, and the input mode it implies (§6.1).
//!
//! §6.1 is modal "only where text entry or confirmation requires it", so the mode
//! is not a variable the reducer sets — it is *derived* from what is open. That is
//! what makes it impossible to be in `ConfirmProcessAction` mode with no pending
//! action, or in `FilterEdit` with nowhere to put the characters.
//!
//! # A stack, not a flag
//!
//! Overlays legitimately nest: §6.2 binds `x` in the process-detail overlay as
//! well as in the table, so a confirmation can sit on top of a detail view, and
//! `Esc` has to peel exactly one layer. What the stack does *not* allow is two
//! overlays of the same kind, which is why [`OverlayStack::push`] replaces rather
//! than appends. The depth is therefore bounded by [`OverlayKind::COUNT`] without
//! an arbitrary limit that could silently swallow a legitimate overlay (§10.3).
//!
//! # The sort selector has no mode of its own
//!
//! §6.1 lists seven modes and the sort selector (`s`) is not among them, so it
//! runs in `Normal` mode and reuses the list bindings: `j`/`k` move the highlight
//! and `Enter` picks. The reducer reads the top of the stack to decide what those
//! actions mean, which is also why the selector is *transient* — opening any other
//! overlay dismisses it rather than burying it.
//!
//! # Process control is a chain, never a keypress
//!
//! [`ProcessActionStage`] is the §15.1 chain made explicit: choose, then confirm.
//! Only [`ProcessActionStage::Confirm`] holds a [`PendingProcessAction`], and only
//! a [`PendingProcessAction`] can become
//! [`crate::action::Effect::SignalProcess`] — via
//! [`PendingProcessAction::into_effect`], after
//! [`crate::action::ConfirmationKind::accepts`] agreed.

use monitrs_core::model::ProcessIdentity;

use crate::action::{PendingProcessAction, SignalKind};
use crate::keymap::InputMode;

use super::text::TextInput;

/// The lowest nice value the renice dialog offers.
///
/// POSIX allows `-20`, but lowering the value needs privileges monitrs must not
/// escalate to (§15.1), so the dialog can *ask* for it and the executor reports
/// the refusal.
pub const MIN_NICE: i8 = -20;

/// The highest nice value the renice dialog offers.
pub const MAX_NICE: i8 = 19;

/// Which stage of the §15.1 confirmation chain a process action is at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessActionStage {
    /// Choosing a signal from the §9.2 dialog order (`x`).
    ChooseSignal {
        /// The process the dialog was opened for.
        identity: ProcessIdentity,
        /// Index into [`SignalKind::DIALOG_ORDER`].
        cursor: usize,
    },
    /// Choosing a nice value (`R`).
    ChooseNice {
        /// The process the dialog was opened for.
        identity: ProcessIdentity,
        /// The requested value, always within [`MIN_NICE`]`..=`[`MAX_NICE`].
        nice: i8,
    },
    /// A concrete action waiting for the confirmation §15.1 requires.
    Confirm(PendingProcessAction),
}

impl ProcessActionStage {
    /// The process this stage is about.
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        match self {
            Self::ChooseSignal { identity, .. } | Self::ChooseNice { identity, .. } => *identity,
            Self::Confirm(pending) => pending.identity(),
        }
    }

    /// The action awaiting confirmation, if this stage is the confirmation.
    ///
    /// `None` for the choosing stages on purpose: a half-made choice must not be
    /// visible as a pending action, or a stray confirmation could execute it.
    #[must_use]
    pub const fn pending(&self) -> Option<PendingProcessAction> {
        match self {
            Self::Confirm(pending) => Some(*pending),
            Self::ChooseSignal { .. } | Self::ChooseNice { .. } => None,
        }
    }

    /// The signal currently highlighted, for the choosing stage.
    #[must_use]
    pub fn highlighted_signal(&self) -> Option<SignalKind> {
        match self {
            Self::ChooseSignal { cursor, .. } => SignalKind::DIALOG_ORDER.get(*cursor).copied(),
            Self::ChooseNice { .. } | Self::Confirm(_) => None,
        }
    }

    /// The action this stage would confirm.
    ///
    /// A choosing stage resolves its current highlight; the confirmation stage is
    /// already resolved. An out-of-range signal cursor falls back to `SIGTERM`,
    /// which is the least forceful option in [`SignalKind::DIALOG_ORDER`] — a
    /// clamped cursor must never silently become `SIGKILL`.
    #[must_use]
    pub fn resolved(&self) -> PendingProcessAction {
        match self {
            Self::ChooseSignal { identity, .. } => PendingProcessAction::Signal {
                identity: *identity,
                signal: self.highlighted_signal().unwrap_or(SignalKind::Term),
            },
            Self::ChooseNice { identity, nice } => PendingProcessAction::Renice {
                identity: *identity,
                nice: *nice,
            },
            Self::Confirm(pending) => *pending,
        }
    }

    /// Moves the highlight, or adjusts the nice value, by `delta`.
    ///
    /// Reports whether anything changed so the reducer can skip a redraw (§16.1).
    pub(in crate::app) fn step(&mut self, delta: i32) -> bool {
        match self {
            Self::ChooseSignal { cursor, .. } => {
                let last = SignalKind::DIALOG_ORDER.len().saturating_sub(1);
                let current = i64::try_from(*cursor).unwrap_or(i64::MAX);
                let target = current.saturating_add(i64::from(delta)).max(0);
                let target = usize::try_from(target).unwrap_or(usize::MAX).min(last);
                if target == *cursor {
                    return false;
                }
                *cursor = target;
                true
            }
            Self::ChooseNice { nice, .. } => {
                let target = i32::from(*nice)
                    .saturating_add(delta)
                    .clamp(i32::from(MIN_NICE), i32::from(MAX_NICE));
                let target = i8::try_from(target).unwrap_or(0);
                if target == *nice {
                    return false;
                }
                *nice = target;
                true
            }
            Self::Confirm(_) => false,
        }
    }
}

/// Which overlay is open. One variant per stack slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OverlayKind {
    /// The generated help (§7.6).
    Help,
    /// The process detail overlay (§7.5).
    ProcessDetail,
    /// The filter editor (§6.2 `/`).
    FilterEdit,
    /// The command palette (§6.3).
    CommandPalette,
    /// The sort selector (§6.2 `s`).
    SortSelector,
    /// A process action mid-chain (§15.1).
    ProcessAction,
}

impl OverlayKind {
    /// Every kind, which is also the maximum stack depth.
    pub const ALL: [Self; 6] = [
        Self::Help,
        Self::ProcessDetail,
        Self::FilterEdit,
        Self::CommandPalette,
        Self::SortSelector,
        Self::ProcessAction,
    ];

    /// The number of kinds, and therefore the bound on stack depth (§10.3).
    pub const COUNT: usize = 6;

    /// The panel title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Help => "HELP",
            Self::ProcessDetail => "PROCESS",
            Self::FilterEdit => "FILTER",
            Self::CommandPalette => "COMMAND",
            Self::SortSelector => "SORT",
            Self::ProcessAction => "CONFIRM",
        }
    }

    /// The mode this overlay puts the app in (§6.1).
    ///
    /// The sort selector resolves to `Normal`: §6.1 defines no mode for it, and
    /// borrowing another mode's table would rebind `j`/`k` to something else.
    #[must_use]
    pub const fn input_mode(self) -> InputMode {
        match self {
            Self::Help => InputMode::Help,
            Self::ProcessDetail => InputMode::ProcessDetail,
            Self::FilterEdit => InputMode::FilterEdit,
            Self::CommandPalette => InputMode::CommandPalette,
            Self::SortSelector => InputMode::Normal,
            Self::ProcessAction => InputMode::ConfirmProcessAction,
        }
    }

    /// Whether opening another overlay should dismiss this one rather than bury
    /// it.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::SortSelector)
    }
}

/// One open overlay, with its own state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Overlay {
    /// The generated help, scrolled by the list bindings (§7.6).
    Help {
        /// First visible line.
        scroll: usize,
    },
    /// The detail of one process (§7.5).
    ProcessDetail {
        /// Whose detail is shown. Carried so a late reply for another process can
        /// be discarded rather than rendered against the wrong row.
        identity: ProcessIdentity,
        /// First visible line.
        scroll: usize,
    },
    /// Editing the process filter (§6.2 `/`).
    FilterEdit {
        /// The buffer. Applied on submit, discarded on `Esc`.
        input: TextInput,
    },
    /// Editing a palette command (§6.3).
    CommandPalette {
        /// The buffer.
        input: TextInput,
        /// Index of the highlighted suggestion.
        highlight: usize,
    },
    /// Picking a sort column (§6.2 `s`).
    SortSelector {
        /// Index into [`monitrs_core::process::ProcessSortKey::ALL`].
        cursor: usize,
    },
    /// A process action mid-chain (§15.1).
    ProcessAction(ProcessActionStage),
}

impl Overlay {
    /// Which kind this is.
    #[must_use]
    pub const fn kind(&self) -> OverlayKind {
        match self {
            Self::Help { .. } => OverlayKind::Help,
            Self::ProcessDetail { .. } => OverlayKind::ProcessDetail,
            Self::FilterEdit { .. } => OverlayKind::FilterEdit,
            Self::CommandPalette { .. } => OverlayKind::CommandPalette,
            Self::SortSelector { .. } => OverlayKind::SortSelector,
            Self::ProcessAction(_) => OverlayKind::ProcessAction,
        }
    }

    /// The mode this overlay implies (§6.1).
    #[must_use]
    pub const fn input_mode(&self) -> InputMode {
        self.kind().input_mode()
    }

    /// The text buffer, for the modes that have one.
    #[must_use]
    pub const fn text_input(&self) -> Option<&TextInput> {
        match self {
            Self::FilterEdit { input } | Self::CommandPalette { input, .. } => Some(input),
            _ => None,
        }
    }

    /// The text buffer, mutably.
    pub(in crate::app) const fn text_input_mut(&mut self) -> Option<&mut TextInput> {
        match self {
            Self::FilterEdit { input } | Self::CommandPalette { input, .. } => Some(input),
            _ => None,
        }
    }

    /// The scroll offset, for the scrollable overlays.
    #[must_use]
    pub const fn scroll(&self) -> Option<usize> {
        match self {
            Self::Help { scroll } | Self::ProcessDetail { scroll, .. } => Some(*scroll),
            _ => None,
        }
    }
}

/// The open overlays, innermost last.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverlayStack {
    overlays: Vec<Overlay>,
}

impl OverlayStack {
    /// Nothing open.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            overlays: Vec::new(),
        }
    }

    /// The overlays, outermost first.
    #[must_use]
    pub fn as_slice(&self) -> &[Overlay] {
        &self.overlays
    }

    /// The topmost overlay, which is the one that owns the keyboard.
    #[must_use]
    pub fn top(&self) -> Option<&Overlay> {
        self.overlays.last()
    }

    /// The topmost overlay, mutably.
    pub(in crate::app) fn top_mut(&mut self) -> Option<&mut Overlay> {
        self.overlays.last_mut()
    }

    /// Whether anything is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty()
    }

    /// How many overlays are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.overlays.len()
    }

    /// Whether an overlay of `kind` is open at any depth.
    #[must_use]
    pub fn contains(&self, kind: OverlayKind) -> bool {
        self.overlays.iter().any(|overlay| overlay.kind() == kind)
    }

    /// The open overlay of `kind`, if any.
    #[must_use]
    pub fn find(&self, kind: OverlayKind) -> Option<&Overlay> {
        self.overlays.iter().find(|overlay| overlay.kind() == kind)
    }

    /// The mode the stack implies, or `None` when nothing is open.
    #[must_use]
    pub fn input_mode(&self) -> Option<InputMode> {
        self.top().map(Overlay::input_mode)
    }

    /// Opens `overlay`.
    ///
    /// Any existing overlay of the same kind is replaced rather than duplicated,
    /// and a transient overlay on top is dismissed rather than buried. Both rules
    /// keep the depth bounded by [`OverlayKind::COUNT`] (§10.3).
    pub(in crate::app) fn push(&mut self, overlay: Overlay) {
        let kind = overlay.kind();
        self.overlays.retain(|open| open.kind() != kind);
        if self
            .overlays
            .last()
            .is_some_and(|open| open.kind().is_transient())
        {
            self.overlays.pop();
        }
        self.overlays.push(overlay);
    }

    /// Closes the topmost overlay, returning it (`Esc`).
    pub(in crate::app) fn pop(&mut self) -> Option<Overlay> {
        self.overlays.pop()
    }

    /// Closes the overlay of `kind`, wherever it is in the stack.
    pub(in crate::app) fn remove(&mut self, kind: OverlayKind) -> bool {
        let before = self.overlays.len();
        self.overlays.retain(|open| open.kind() != kind);
        self.overlays.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(31_842, 900_100)
    }

    #[test]
    fn the_mode_is_derived_from_whatever_is_on_top() {
        let mut stack = OverlayStack::new();
        assert_eq!(
            stack.input_mode(),
            None,
            "no overlay implies no mode change"
        );

        stack.push(Overlay::Help { scroll: 0 });
        assert_eq!(stack.input_mode(), Some(InputMode::Help));

        stack.push(Overlay::ProcessDetail {
            identity: identity(),
            scroll: 0,
        });
        assert_eq!(stack.input_mode(), Some(InputMode::ProcessDetail));

        stack.push(Overlay::ProcessAction(ProcessActionStage::ChooseSignal {
            identity: identity(),
            cursor: 0,
        }));
        assert_eq!(stack.input_mode(), Some(InputMode::ConfirmProcessAction));

        assert!(stack.pop().is_some());
        assert_eq!(
            stack.input_mode(),
            Some(InputMode::ProcessDetail),
            "Esc peels exactly one layer"
        );
    }

    #[test]
    fn every_kind_maps_to_a_mode_and_a_title() {
        for kind in OverlayKind::ALL {
            assert!(!kind.title().is_empty());
            assert!(kind.title().is_ascii(), "§5.1: titles must be ASCII-safe");
        }
        assert_eq!(OverlayKind::ALL.len(), OverlayKind::COUNT);
        assert_eq!(
            OverlayKind::SortSelector.input_mode(),
            InputMode::Normal,
            "§6.1 defines no mode for the sort selector"
        );
    }

    #[test]
    fn the_same_overlay_kind_never_stacks_twice() {
        let mut stack = OverlayStack::new();
        for _ in 0..10 {
            stack.push(Overlay::Help { scroll: 0 });
        }
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn the_stack_depth_is_bounded_by_the_number_of_kinds() {
        let mut stack = OverlayStack::new();
        for _ in 0..4 {
            stack.push(Overlay::Help { scroll: 0 });
            stack.push(Overlay::ProcessDetail {
                identity: identity(),
                scroll: 0,
            });
            stack.push(Overlay::FilterEdit {
                input: TextInput::new(),
            });
            stack.push(Overlay::CommandPalette {
                input: TextInput::new(),
                highlight: 0,
            });
            stack.push(Overlay::ProcessAction(ProcessActionStage::Confirm(
                PendingProcessAction::Signal {
                    identity: identity(),
                    signal: SignalKind::Term,
                },
            )));
        }
        assert!(stack.len() <= OverlayKind::COUNT);
    }

    #[test]
    fn a_transient_selector_is_dismissed_rather_than_buried() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::SortSelector { cursor: 2 });
        stack.push(Overlay::FilterEdit {
            input: TextInput::new(),
        });

        assert_eq!(stack.len(), 1);
        assert!(!stack.contains(OverlayKind::SortSelector));
        assert_eq!(stack.input_mode(), Some(InputMode::FilterEdit));
    }

    #[test]
    fn only_the_confirmation_stage_exposes_a_pending_action() {
        let choosing = ProcessActionStage::ChooseSignal {
            identity: identity(),
            cursor: 3,
        };
        assert_eq!(
            choosing.pending(),
            None,
            "§15.1: a half-made choice is not a pending action"
        );
        assert_eq!(choosing.highlighted_signal(), Some(SignalKind::Kill));
        assert_eq!(
            choosing.resolved(),
            PendingProcessAction::Signal {
                identity: identity(),
                signal: SignalKind::Kill,
            }
        );

        let confirming = ProcessActionStage::Confirm(PendingProcessAction::Signal {
            identity: identity(),
            signal: SignalKind::Kill,
        });
        assert!(confirming.pending().is_some());
        assert_eq!(confirming.identity(), identity());
    }

    #[test]
    fn an_out_of_range_signal_cursor_falls_back_to_the_least_forceful_signal() {
        let broken = ProcessActionStage::ChooseSignal {
            identity: identity(),
            cursor: 99,
        };
        assert_eq!(broken.highlighted_signal(), None);
        assert_eq!(
            broken.resolved(),
            PendingProcessAction::Signal {
                identity: identity(),
                signal: SignalKind::Term,
            },
            "a clamped cursor must never become SIGKILL"
        );
    }

    #[test]
    fn the_signal_cursor_clamps_to_the_dialog_order() {
        let mut stage = ProcessActionStage::ChooseSignal {
            identity: identity(),
            cursor: 0,
        };
        assert!(!stage.step(-1), "already at the first choice");
        assert!(stage.step(1));
        assert_eq!(stage.highlighted_signal(), Some(SignalKind::Int));
        assert!(stage.step(10));
        assert_eq!(
            stage.highlighted_signal(),
            Some(SignalKind::Kill),
            "SIGKILL is last (§9.2)"
        );
        assert!(!stage.step(1));
    }

    #[test]
    fn the_nice_value_clamps_to_the_posix_range() {
        let mut stage = ProcessActionStage::ChooseNice {
            identity: identity(),
            nice: 0,
        };
        assert!(stage.step(5));
        assert_eq!(
            stage.resolved(),
            PendingProcessAction::Renice {
                identity: identity(),
                nice: 5,
            }
        );
        assert!(stage.step(1_000));
        assert_eq!(
            stage.resolved(),
            PendingProcessAction::Renice {
                identity: identity(),
                nice: MAX_NICE,
            }
        );
        assert!(!stage.step(1));
        assert!(stage.step(-1_000));
        assert_eq!(
            stage.resolved(),
            PendingProcessAction::Renice {
                identity: identity(),
                nice: MIN_NICE,
            }
        );
        assert!(!stage.step(-1));
    }

    #[test]
    fn a_confirmation_stage_has_nothing_to_step_through() {
        let mut stage = ProcessActionStage::Confirm(PendingProcessAction::Renice {
            identity: identity(),
            nice: 3,
        });
        assert!(!stage.step(1));
        assert!(!stage.step(-1));
    }

    #[test]
    fn text_overlays_expose_their_buffer_and_others_do_not() {
        let mut palette = Overlay::CommandPalette {
            input: TextInput::seeded("sort cpu"),
            highlight: 0,
        };
        assert_eq!(palette.text_input().map(TextInput::text), Some("sort cpu"));
        assert!(palette.text_input_mut().is_some());

        let mut help = Overlay::Help { scroll: 4 };
        assert!(help.text_input().is_none());
        assert!(help.text_input_mut().is_none());
        assert_eq!(help.scroll(), Some(4));
        assert_eq!(palette.scroll(), None);
    }

    #[test]
    fn removing_reports_whether_anything_happened() {
        let mut stack = OverlayStack::new();
        assert!(!stack.remove(OverlayKind::Help));

        stack.push(Overlay::Help { scroll: 0 });
        stack.push(Overlay::SortSelector { cursor: 0 });
        assert!(stack.remove(OverlayKind::Help));
        assert_eq!(stack.len(), 1);
        assert!(stack.find(OverlayKind::SortSelector).is_some());
        assert!(stack.pop().is_some());
        assert!(stack.is_empty());
        assert!(stack.pop().is_none());
    }
}
