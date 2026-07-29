//! The keymap: modes, the complete §6.2 default bindings, mode-aware resolution,
//! conflict detection (§12) and the generated help (§7.6).
//!
//! # One table, three consumers
//!
//! The bindings below are the single source of truth for what a key does, what
//! help says a key does, and what a configuration file is allowed to override.
//! §7.6 is explicit that help must be *generated* from the active keymap rather
//! than maintained beside it, so [`Keymap::help`] reads the same rows the
//! resolver reads. A test asserts the coverage is total.
//!
//! # Mode-aware resolution
//!
//! §6.1's modes exist because text entry and confirmation need different
//! meanings for the same keys. `q` quits in `Normal` and types a literal `q` in
//! `FilterEdit`; `Ctrl-U` pages up in a list and clears the line in an editor;
//! `Home` goes to the first row in a list and to the oldest sample in the Time
//! Lens. A binding therefore carries the set of modes it applies to, and the same
//! key legitimately appears in several rows as long as no two rows claim it in
//! the *same* mode — which is exactly what [`Keymap::validate`] rejects.
//!
//! # Process control is never one keypress
//!
//! §15.1 is absolute: no signal from a single accidental keypress. `x`, `T`, `K`
//! and `R` therefore resolve to *proposals*
//! ([`Action::is_process_action_proposal`]) which open `ConfirmProcessAction`
//! mode. The only actions that can end in [`crate::action::Effect::SignalProcess`]
//! are the confirmations, and they are bound in that mode alone. There is a test
//! that walks every binding in every mode to prove it.
//!
//! # `g` is both a binding and a prefix
//!
//! §6.2 binds `g` to *cycle glyph mode* and `gg` to *first row*. The ambiguity is
//! resolved the way a modal editor resolves it: `g` starts a pending sequence, a
//! second `g` completes it, and anything else — another key, or
//! [`KeyResolver::timeout`] elapsing — releases `g`'s own action first. Nothing is
//! silently swallowed.

use core::time::Duration;
use std::collections::HashMap;
use std::time::Instant;

use crate::action::{Action, Seek, SignalKind, ViewId};
use crate::event::{Key, KeyPress};

/// The input modes of §6.1.
///
/// Modal only where text entry or confirmation requires it: everything else is
/// reachable from `Normal`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InputMode {
    /// Browsing. Every non-modal binding is live here.
    Normal,
    /// Editing the process filter (§6.2 `/`).
    FilterEdit,
    /// Editing a palette command (§6.3).
    CommandPalette,
    /// A destructive process action is waiting for confirmation (§15.1).
    ConfirmProcessAction,
    /// The generated help overlay is open (§7.6).
    Help,
    /// The process detail overlay is open (§7.5).
    ProcessDetail,
    /// The Time Lens has focus, so the arrow keys seek through history (§2.1).
    TimeLens,
}

impl InputMode {
    /// Every mode. Used by the exhaustiveness tests and by help generation.
    pub const ALL: [Self; 7] = [
        Self::Normal,
        Self::FilterEdit,
        Self::CommandPalette,
        Self::ConfirmProcessAction,
        Self::Help,
        Self::ProcessDetail,
        Self::TimeLens,
    ];

    /// The name shown in the status line and in help headings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::FilterEdit => "filter",
            Self::CommandPalette => "command palette",
            Self::ConfirmProcessAction => "confirm",
            Self::Help => "help",
            Self::ProcessDetail => "process detail",
            Self::TimeLens => "time lens",
        }
    }

    /// Whether the mode is editing text, i.e. whether §6.2's "unless editing
    /// text" exception applies.
    #[must_use]
    pub const fn is_text_entry(self) -> bool {
        matches!(self, Self::FilterEdit | Self::CommandPalette)
    }

    /// Whether the mode is an overlay that `Esc` closes.
    #[must_use]
    pub const fn is_overlay(self) -> bool {
        matches!(
            self,
            Self::FilterEdit
                | Self::CommandPalette
                | Self::ConfirmProcessAction
                | Self::Help
                | Self::ProcessDetail
        )
    }

    /// The bit this mode occupies in a [`ModeSet`].
    const fn bit(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::FilterEdit => 1 << 1,
            Self::CommandPalette => 1 << 2,
            Self::ConfirmProcessAction => 1 << 3,
            Self::Help => 1 << 4,
            Self::ProcessDetail => 1 << 5,
            Self::TimeLens => 1 << 6,
        }
    }
}

/// A set of [`InputMode`]s, so one binding row can serve several modes.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ModeSet(u8);

impl ModeSet {
    /// No modes. A binding with an empty set is a bug [`Keymap::validate`]
    /// rejects.
    pub const EMPTY: Self = Self(0);

    /// Every mode.
    pub const ALL: Self = Self(0b0111_1111);

    /// This set plus `mode`.
    #[must_use]
    pub const fn with(self, mode: InputMode) -> Self {
        Self(self.0 | mode.bit())
    }

    /// This set minus `mode`.
    #[must_use]
    pub const fn without(self, mode: InputMode) -> Self {
        Self(self.0 & !mode.bit())
    }

    /// Whether `mode` is a member.
    #[must_use]
    pub const fn contains(self, mode: InputMode) -> bool {
        self.0 & mode.bit() != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Members, in [`InputMode::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = InputMode> {
        InputMode::ALL
            .into_iter()
            .filter(move |mode| self.contains(*mode))
    }
}

/// Every mode: only `Ctrl-C` and `Esc` are this universal (§6.2).
const ANY_MODE: ModeSet = ModeSet::ALL;

/// Every mode that is not editing text, i.e. where `q` quits (§6.2).
const QUIT_MODES: ModeSet = ModeSet::ALL
    .without(InputMode::FilterEdit)
    .without(InputMode::CommandPalette);

/// The two modes where the main view has focus, and therefore where view
/// switching, pausing, seeking and the display toggles apply.
const VIEW_MODES: ModeSet = ModeSet::EMPTY
    .with(InputMode::Normal)
    .with(InputMode::TimeLens);

/// The modes with a scrollable list or table: the process table, the help
/// overlay and the detail overlay (§6.2 "Lists and tables").
const NAV_MODES: ModeSet = ModeSet::EMPTY
    .with(InputMode::Normal)
    .with(InputMode::Help)
    .with(InputMode::ProcessDetail);

/// `Normal` only: bindings that act on the process table itself.
const TABLE_MODE: ModeSet = ModeSet::EMPTY.with(InputMode::Normal);

/// Where a process can be proposed for an action: the table, and the detail
/// overlay of the very process being looked at.
const PROCESS_MODES: ModeSet = ModeSet::EMPTY
    .with(InputMode::Normal)
    .with(InputMode::ProcessDetail);

/// The confirmation dialog.
const CONFIRM_MODE: ModeSet = ModeSet::EMPTY.with(InputMode::ConfirmProcessAction);

/// The two text editors.
const TEXT_MODES: ModeSet = ModeSet::EMPTY
    .with(InputMode::FilterEdit)
    .with(InputMode::CommandPalette);

/// The palette only, which has a suggestion list the filter editor lacks.
const PALETTE_MODE: ModeSet = ModeSet::EMPTY.with(InputMode::CommandPalette);

/// The help overlay only.
const HELP_MODE: ModeSet = ModeSet::EMPTY.with(InputMode::Help);

/// The Time Lens only, where the arrow keys seek instead of selecting (§2.1).
const LENS_MODE: ModeSet = ModeSet::EMPTY.with(InputMode::TimeLens);

/// Which §6.2 table a binding belongs to. Also the help section it appears under.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BindingSection {
    /// §6.2 "Global".
    Global,
    /// §6.2 "Lists and tables".
    Navigation,
    /// §2.1 history navigation.
    TimeLens,
    /// §6.2 "Process control", plus the confirmation dialog itself.
    ProcessControl,
    /// Editing the filter or a palette command (§6.1).
    TextEntry,
}

impl BindingSection {
    /// Sections in the order help lists them.
    pub const ORDER: [Self; 5] = [
        Self::Global,
        Self::Navigation,
        Self::TimeLens,
        Self::ProcessControl,
        Self::TextEntry,
    ];

    /// The heading shown in generated help.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Navigation => "Lists and tables",
            Self::TimeLens => "Time Lens",
            Self::ProcessControl => "Process control",
            Self::TextEntry => "Text entry",
        }
    }
}

/// What a keypress has to look like to match a binding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeyPattern {
    /// Exactly this normalized press.
    Exact(KeyPress),
    /// Any key that produces a character, with no `Ctrl` or `Alt`.
    ///
    /// This is how `FilterEdit` types a literal `q` without enumerating every
    /// character. An [`KeyPattern::Exact`] binding in the same mode wins, so
    /// `Enter` and `Ctrl-C` keep their meanings while editing (§6.1).
    PrintableChar,
}

impl KeyPattern {
    /// Whether `press` matches this pattern.
    #[must_use]
    pub fn matches(&self, press: KeyPress) -> bool {
        match self {
            Self::Exact(expected) => *expected == KeyPress::new(press.key, press.modifiers),
            Self::PrintableChar => press.typed_char().is_some(),
        }
    }

    /// The label used in generated help.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Exact(press) => press.label(),
            Self::PrintableChar => "any character".to_owned(),
        }
    }
}

/// One or two keys that together select a binding.
///
/// Two is the limit: §6.2's only sequence is `gg`, and a deeper prefix tree would
/// mean holding more input in a mode-dependent buffer for longer, which is the
/// opposite of the responsive-input goal in §3.1.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Chord {
    /// The first key.
    pub first: KeyPattern,
    /// The second key, for a sequence like `gg`.
    pub second: Option<KeyPattern>,
}

impl Chord {
    /// A single-key chord. The press is normalized so a table entry and a real
    /// key press cannot disagree about modifiers (§6.2).
    #[must_use]
    pub const fn key(press: KeyPress) -> Self {
        Self {
            first: KeyPattern::Exact(KeyPress::new(press.key, press.modifiers)),
            second: None,
        }
    }

    /// A two-key sequence, e.g. `gg`.
    #[must_use]
    pub const fn pair(first: KeyPress, second: KeyPress) -> Self {
        Self {
            first: KeyPattern::Exact(KeyPress::new(first.key, first.modifiers)),
            second: Some(KeyPattern::Exact(KeyPress::new(
                second.key,
                second.modifiers,
            ))),
        }
    }

    /// The catch-all used by text entry.
    #[must_use]
    pub const fn printable() -> Self {
        Self {
            first: KeyPattern::PrintableChar,
            second: None,
        }
    }

    /// Whether this chord needs a second key.
    #[must_use]
    pub const fn is_sequence(&self) -> bool {
        self.second.is_some()
    }

    /// The label used in generated help: `gg`, `Ctrl-C`, `Shift-Tab`.
    #[must_use]
    pub fn label(&self) -> String {
        let first = self.first.label();
        match &self.second {
            None => first,
            Some(second) => {
                let second = second.label();
                // `gg` reads as one token; `g Home` would not.
                if first.chars().count() == 1 && second.chars().count() == 1 {
                    format!("{first}{second}")
                } else {
                    format!("{first} {second}")
                }
            }
        }
    }
}

/// What a matched binding produces.
#[derive(Clone, Debug, PartialEq)]
pub enum BindingOutcome {
    /// Always this action.
    Fixed(Action),
    /// Insert the character that was actually typed. Only valid together with
    /// [`KeyPattern::PrintableChar`], which [`Keymap::validate`] enforces.
    InsertTypedChar,
}

impl BindingOutcome {
    /// A short name for diagnostics, so a conflict error can name both sides
    /// (§12 requires key conflicts to be detected; §21 M6 requires them to be
    /// rejected with a useful message).
    #[must_use]
    pub fn diagnostic_name(&self) -> String {
        match self {
            Self::Fixed(action) => format!("{action:?}"),
            Self::InsertTypedChar => "InsertChar".to_owned(),
        }
    }
}

/// One row of the keymap.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    /// The keys that select it.
    pub chord: Chord,
    /// The modes it applies to.
    pub modes: ModeSet,
    /// What it produces.
    pub outcome: BindingOutcome,
    /// Which help section it belongs to.
    pub section: BindingSection,
    /// The help text. One clause, imperative, no trailing period.
    pub description: &'static str,
}

impl Binding {
    /// A fixed-action binding.
    #[must_use]
    pub const fn new(
        section: BindingSection,
        modes: ModeSet,
        chord: Chord,
        action: Action,
        description: &'static str,
    ) -> Self {
        Self {
            chord,
            modes,
            outcome: BindingOutcome::Fixed(action),
            section,
            description,
        }
    }

    /// The action this binding produces for `press`.
    ///
    /// `press` is the key that completed the chord, which is what
    /// [`BindingOutcome::InsertTypedChar`] needs. Returns `None` only for the
    /// combination [`Keymap::validate`] rejects, so no code path panics on a
    /// hand-written keymap.
    #[must_use]
    pub fn resolved_action(&self, press: KeyPress) -> Option<Action> {
        match &self.outcome {
            BindingOutcome::Fixed(action) => Some(action.clone()),
            BindingOutcome::InsertTypedChar => press.typed_char().map(Action::InsertChar),
        }
    }

    /// The fixed action, if this binding has one.
    #[must_use]
    pub const fn action(&self) -> Option<&Action> {
        match &self.outcome {
            BindingOutcome::Fixed(action) => Some(action),
            BindingOutcome::InsertTypedChar => None,
        }
    }
}

/// A keymap that cannot be used (§12, §21 M6: conflicts are rejected).
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KeymapError {
    /// Two bindings claim the same keys in the same mode.
    #[error("key `{key}` is bound twice in {mode} mode: {first} and {second}")]
    Conflict {
        /// The mode both bindings claim.
        mode: &'static str,
        /// The rendered chord, e.g. `gg`.
        key: String,
        /// The first binding's action.
        first: String,
        /// The second binding's action.
        second: String,
    },
    /// A binding is malformed.
    #[error("binding `{key}` is invalid: {reason}")]
    InvalidBinding {
        /// The rendered chord.
        key: String,
        /// Why it cannot be used.
        reason: &'static str,
    },
}

/// One line of generated help: the keys, and what they do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    /// Every chord that produces this action in this mode, e.g. `j / Down`.
    pub keys: String,
    /// The binding description.
    pub description: &'static str,
}

/// A titled group of help entries, mirroring the §6.2 tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpSection {
    /// The heading.
    pub title: &'static str,
    /// The entries, in table order.
    pub entries: Vec<HelpEntry>,
}

/// The bindings of §6.2, plus the text-entry and confirmation bindings the modes
/// of §6.1 imply.
#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    /// The default keymap.
    ///
    /// Does not validate, because a `Result` here would push a
    /// `expect`-shaped panic into every caller for a table that cannot change at
    /// runtime. Its correctness is pinned by
    /// `the_builtin_keymap_has_no_conflicts` instead, and resolution is
    /// first-match-wins, so even a hypothetical duplicate degrades into a shadowed
    /// row rather than a panic (§18.2: no panics in production paths).
    #[must_use]
    pub fn builtin() -> Self {
        let mut table = Table::default();

        // ---------------------------------------------------------------- global
        // §6.2: "Quit, unless editing text". The exception is textual entry only,
        // so `q` also quits from an open confirmation dialog — which is safe,
        // because quitting sends no signal.
        table.add(
            BindingSection::Global,
            QUIT_MODES,
            Chord::key(KeyPress::char('q')),
            Action::Quit,
            "Quit",
        );
        table.add(
            BindingSection::Global,
            ANY_MODE,
            Chord::key(KeyPress::ctrl('c')),
            Action::Quit,
            "Quit safely from any mode",
        );
        table.add(
            BindingSection::Global,
            ANY_MODE,
            Chord::key(KeyPress::plain(Key::Escape)),
            Action::CancelOverlay,
            "Close the overlay or cancel the current mode",
        );
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char('?')),
            Action::ToggleHelp,
            "Toggle context-aware help",
        );
        table.add(
            BindingSection::Global,
            HELP_MODE,
            Chord::key(KeyPress::char('?')),
            Action::CancelOverlay,
            "Close help",
        );
        table.add(
            BindingSection::Global,
            TABLE_MODE,
            Chord::key(KeyPress::plain(Key::Tab)),
            Action::NextPanel,
            "Focus the next panel",
        );
        table.add(
            BindingSection::Global,
            TABLE_MODE,
            Chord::key(KeyPress::plain(Key::BackTab)),
            Action::PreviousPanel,
            "Focus the previous panel",
        );
        for view in ViewId::ALL {
            table.add(
                BindingSection::Global,
                VIEW_MODES,
                Chord::key(KeyPress::char(view.digit())),
                Action::ChangeView(view),
                match view {
                    ViewId::Overview => "Go to the Overview view",
                    ViewId::Processes => "Go to the Processes view",
                    ViewId::Storage => "Go to the Storage view",
                    ViewId::Network => "Go to the Network view",
                    ViewId::Inspect => "Go to the Inspect view",
                },
            );
        }
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char(' ')),
            Action::TogglePause,
            "Pause or resume the visible timeline",
        );
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char('L')),
            Action::ReturnLive,
            "Return to the live sample",
        );
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char(':')),
            Action::OpenCommandPalette,
            "Open the command palette",
        );
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char('r')),
            Action::ForceRefresh,
            "Force a refresh",
        );
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char('t')),
            Action::CycleTheme,
            "Cycle the theme",
        );
        table.add(
            BindingSection::Global,
            VIEW_MODES,
            Chord::key(KeyPress::char('g')),
            Action::CycleGlyphMode,
            "Cycle the glyph mode",
        );

        // ------------------------------------------------------ lists and tables
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::char('j')),
            Action::SelectNext,
            "Next row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::plain(Key::Down)),
            Action::SelectNext,
            "Next row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::char('k')),
            Action::SelectPrevious,
            "Previous row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::plain(Key::Up)),
            Action::SelectPrevious,
            "Previous row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::ctrl('d')),
            Action::SelectPageDown,
            "Page down",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::plain(Key::PageDown)),
            Action::SelectPageDown,
            "Page down",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::ctrl('u')),
            Action::SelectPageUp,
            "Page up",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::plain(Key::PageUp)),
            Action::SelectPageUp,
            "Page up",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::pair(KeyPress::char('g'), KeyPress::char('g')),
            Action::SelectFirst,
            "First row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::plain(Key::Home)),
            Action::SelectFirst,
            "First row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::char('G')),
            Action::SelectLast,
            "Last row",
        );
        table.add(
            BindingSection::Navigation,
            NAV_MODES,
            Chord::key(KeyPress::plain(Key::End)),
            Action::SelectLast,
            "Last row",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::plain(Key::Enter)),
            Action::InspectSelected,
            "Inspect the selected item",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('/')),
            Action::BeginFilterEdit,
            "Edit the filter",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('n')),
            Action::NextMatch,
            "Next filter match",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('N')),
            Action::PreviousMatch,
            "Previous filter match",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('s')),
            Action::OpenSortSelector,
            "Open the sort selector",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('S')),
            Action::ReverseSort,
            "Reverse the current sort",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('p')),
            Action::PinSelected,
            "Pin or unpin the selected process",
        );
        table.add(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord::key(KeyPress::char('f')),
            Action::ToggleTreeView,
            "Toggle flat and tree process views",
        );

        // ------------------------------------------------------------- time lens
        // §2.1: `[` and `]` step, and their shifted forms leap. Both work from the
        // main view; the arrow keys only once the Time Lens has focus, because in
        // a list they belong to the selection.
        table.add(
            BindingSection::TimeLens,
            VIEW_MODES,
            Chord::key(KeyPress::char('[')),
            Action::SeekHistory(Seek::step_back()),
            "Step one sample back",
        );
        table.add(
            BindingSection::TimeLens,
            VIEW_MODES,
            Chord::key(KeyPress::char(']')),
            Action::SeekHistory(Seek::step_forward()),
            "Step one sample forward",
        );
        table.add(
            BindingSection::TimeLens,
            VIEW_MODES,
            Chord::key(KeyPress::char('{')),
            Action::SeekHistory(Seek::leap_back()),
            "Leap ten samples back",
        );
        table.add(
            BindingSection::TimeLens,
            VIEW_MODES,
            Chord::key(KeyPress::char('}')),
            Action::SeekHistory(Seek::leap_forward()),
            "Leap ten samples forward",
        );
        table.add(
            BindingSection::TimeLens,
            LENS_MODE,
            Chord::key(KeyPress::plain(Key::Left)),
            Action::SeekHistory(Seek::step_back()),
            "Step one sample back",
        );
        table.add(
            BindingSection::TimeLens,
            LENS_MODE,
            Chord::key(KeyPress::plain(Key::Right)),
            Action::SeekHistory(Seek::step_forward()),
            "Step one sample forward",
        );
        table.add(
            BindingSection::TimeLens,
            LENS_MODE,
            Chord::key(KeyPress::plain(Key::Home)),
            Action::SeekHistory(Seek::Oldest),
            "Jump to the oldest retained sample",
        );
        table.add(
            BindingSection::TimeLens,
            LENS_MODE,
            Chord::key(KeyPress::plain(Key::End)),
            Action::SeekHistory(Seek::Newest),
            "Jump to the newest retained sample",
        );

        // -------------------------------------------------------- process control
        // Proposals only: §15.1 forbids a signal from one keypress.
        table.add(
            BindingSection::ProcessControl,
            PROCESS_MODES,
            Chord::key(KeyPress::char('x')),
            Action::OpenSignalDialog,
            "Open the signal dialog",
        );
        table.add(
            BindingSection::ProcessControl,
            PROCESS_MODES,
            Chord::key(KeyPress::char('T')),
            Action::ProposeSignal(SignalKind::Term),
            "Propose SIGTERM",
        );
        table.add(
            BindingSection::ProcessControl,
            PROCESS_MODES,
            Chord::key(KeyPress::char('K')),
            Action::ProposeSignal(SignalKind::Kill),
            "Propose SIGKILL, never sent immediately",
        );
        table.add(
            BindingSection::ProcessControl,
            PROCESS_MODES,
            Chord::key(KeyPress::char('R')),
            Action::ProposeRenice,
            "Propose a renice, where supported",
        );

        // The confirmation dialog. `Y` is the distinct forceful confirmation
        // §15.1 asks for: distinct from Enter, and deliberately not a second
        // press of `K`, which a double tap would supply by accident.
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::plain(Key::Enter)),
            Action::ConfirmPendingAction,
            "Confirm the pending action",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::char('y')),
            Action::ConfirmPendingAction,
            "Confirm the pending action",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::char('Y')),
            Action::ConfirmForcefulAction,
            "Confirm a forceful action such as SIGKILL",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::char('n')),
            Action::CancelOverlay,
            "Cancel without acting",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::char('j')),
            Action::SelectNext,
            "Next choice",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::plain(Key::Down)),
            Action::SelectNext,
            "Next choice",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::char('k')),
            Action::SelectPrevious,
            "Previous choice",
        );
        table.add(
            BindingSection::ProcessControl,
            CONFIRM_MODE,
            Chord::key(KeyPress::plain(Key::Up)),
            Action::SelectPrevious,
            "Previous choice",
        );

        // ------------------------------------------------------------ text entry
        // The catch-all comes first in the table for readability; resolution
        // prefers exact patterns regardless of order.
        table.add_typed(TEXT_MODES, "Type into the input");
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::Enter)),
            Action::SubmitInput,
            "Apply the input",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::Backspace)),
            Action::DeleteBackward,
            "Delete the character before the cursor",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::Delete)),
            Action::DeleteForward,
            "Delete the character under the cursor",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::ctrl('w')),
            Action::DeleteWordBackward,
            "Delete the previous word",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::ctrl('u')),
            Action::ClearInput,
            "Clear the input",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::Left)),
            Action::MoveCursorLeft,
            "Move the cursor left",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::Right)),
            Action::MoveCursorRight,
            "Move the cursor right",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::Home)),
            Action::MoveCursorToStart,
            "Move the cursor to the start",
        );
        table.add(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::key(KeyPress::plain(Key::End)),
            Action::MoveCursorToEnd,
            "Move the cursor to the end",
        );
        table.add(
            BindingSection::TextEntry,
            PALETTE_MODE,
            Chord::key(KeyPress::plain(Key::Down)),
            Action::SelectNext,
            "Next suggestion",
        );
        table.add(
            BindingSection::TextEntry,
            PALETTE_MODE,
            Chord::key(KeyPress::plain(Key::Up)),
            Action::SelectPrevious,
            "Previous suggestion",
        );

        Self {
            bindings: table.bindings,
        }
    }

    /// A keymap from an explicit table, rejecting conflicts (§12, §21 M6).
    ///
    /// This is the entry point a configured `[keys]` table will use.
    ///
    /// # Errors
    ///
    /// [`KeymapError::Conflict`] if two rows claim the same chord in the same
    /// mode, [`KeymapError::InvalidBinding`] if a row is malformed.
    pub fn from_bindings(bindings: Vec<Binding>) -> Result<Self, KeymapError> {
        let keymap = Self { bindings };
        keymap.validate()?;
        Ok(keymap)
    }

    /// Every row, in table order.
    #[must_use]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// The rows active in `mode`, in table order.
    pub fn bindings_for_mode(&self, mode: InputMode) -> impl Iterator<Item = &Binding> {
        self.bindings
            .iter()
            .filter(move |binding| binding.modes.contains(mode))
    }

    /// Checks the invariants a usable keymap must hold.
    ///
    /// # Errors
    ///
    /// The first problem found, naming the key and both actions so the message
    /// can be shown verbatim in a config error (§12: invalid input points at the
    /// exact key).
    pub fn validate(&self) -> Result<(), KeymapError> {
        let mut claimed: HashMap<(InputMode, Chord), &BindingOutcome> = HashMap::new();

        for binding in &self.bindings {
            if binding.modes.is_empty() {
                return Err(KeymapError::InvalidBinding {
                    key: binding.chord.label(),
                    reason: "the binding applies to no input mode",
                });
            }
            match (&binding.chord, &binding.outcome) {
                (chord, BindingOutcome::InsertTypedChar)
                    if chord.first != KeyPattern::PrintableChar || chord.is_sequence() =>
                {
                    return Err(KeymapError::InvalidBinding {
                        key: chord.label(),
                        reason: "typing a character requires the single printable-key pattern",
                    });
                }
                (chord, BindingOutcome::Fixed(_)) if chord.first == KeyPattern::PrintableChar => {
                    return Err(KeymapError::InvalidBinding {
                        key: chord.label(),
                        reason: "the printable-key pattern can only insert the typed character",
                    });
                }
                (chord, _) if chord.second == Some(KeyPattern::PrintableChar) => {
                    return Err(KeymapError::InvalidBinding {
                        key: chord.label(),
                        reason: "a sequence cannot end in the printable-key pattern",
                    });
                }
                _ => {}
            }

            for mode in binding.modes.iter() {
                if let Some(existing) = claimed.insert((mode, binding.chord), &binding.outcome) {
                    return Err(KeymapError::Conflict {
                        mode: mode.label(),
                        key: binding.chord.label(),
                        first: existing.diagnostic_name(),
                        second: binding.outcome.diagnostic_name(),
                    });
                }
            }
        }

        Ok(())
    }

    /// The row bound to `chord` in `mode`, if any.
    ///
    /// A direct table lookup: it knows nothing about sequence timing, so
    /// interactive input must go through [`KeyResolver`] instead.
    #[must_use]
    pub fn binding(&self, mode: InputMode, chord: &Chord) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|binding| binding.modes.contains(mode) && binding.chord == *chord)
    }

    /// Context-aware help, generated from this keymap (§7.6).
    ///
    /// Only the bindings that are live in `mode` are listed, chords that share a
    /// description are merged (`j / Down`), and the result is a `Vec` so the
    /// overlay can scroll it.
    #[must_use]
    pub fn help(&self, mode: InputMode) -> Vec<HelpSection> {
        let mut sections = Vec::new();

        for section in BindingSection::ORDER {
            let mut entries: Vec<HelpEntry> = Vec::new();
            for binding in self
                .bindings_for_mode(mode)
                .filter(|binding| binding.section == section)
            {
                let label = binding.chord.label();
                match entries
                    .iter_mut()
                    .find(|entry| entry.description == binding.description)
                {
                    Some(entry) => {
                        entry.keys.push_str(" / ");
                        entry.keys.push_str(&label);
                    }
                    None => entries.push(HelpEntry {
                        keys: label,
                        description: binding.description,
                    }),
                }
            }
            if !entries.is_empty() {
                sections.push(HelpSection {
                    title: section.title(),
                    entries,
                });
            }
        }

        sections
    }

    /// The row a single press selects on its own, ignoring sequences.
    fn standalone(&self, mode: InputMode, press: KeyPress) -> Option<&Binding> {
        self.binding(mode, &Chord::key(press))
    }

    /// Whether `press` is the first key of some sequence in `mode`.
    fn starts_sequence(&self, mode: InputMode, press: KeyPress) -> bool {
        let pattern = KeyPattern::Exact(KeyPress::new(press.key, press.modifiers));
        self.bindings.iter().any(|binding| {
            binding.modes.contains(mode)
                && binding.chord.is_sequence()
                && binding.chord.first == pattern
        })
    }

    /// The row the sequence `first` then `second` selects in `mode`.
    fn sequence(&self, mode: InputMode, first: KeyPress, second: KeyPress) -> Option<&Binding> {
        self.binding(mode, &Chord::pair(first, second))
    }

    /// The catch-all text-entry row for `mode`, if `press` produces a character.
    fn typed(&self, mode: InputMode, press: KeyPress) -> Option<&Binding> {
        // A key that types nothing can never match the catch-all.
        press.typed_char()?;
        self.binding(mode, &Chord::printable())
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Accumulates the default table.
#[derive(Debug, Default)]
struct Table {
    bindings: Vec<Binding>,
}

impl Table {
    fn add(
        &mut self,
        section: BindingSection,
        modes: ModeSet,
        chord: Chord,
        action: Action,
        description: &'static str,
    ) {
        self.bindings
            .push(Binding::new(section, modes, chord, action, description));
    }

    fn add_typed(&mut self, modes: ModeSet, description: &'static str) {
        self.bindings.push(Binding {
            chord: Chord::printable(),
            modes,
            outcome: BindingOutcome::InsertTypedChar,
            section: BindingSection::TextEntry,
            description,
        });
    }
}

/// How long an incomplete sequence waits for its second key.
///
/// Long enough for a deliberate `gg`, short enough that `g`'s own action does not
/// feel lost. The app must call [`KeyResolver::poll_timeout`] on its tick
/// (§10.2's `Tick`) for the deadline to be noticed while no key is pressed.
pub const DEFAULT_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(500);

/// What one key press resolved to.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    /// Nothing is bound to this key in this mode.
    Unbound,
    /// One action.
    Action(Action),
    /// Two actions, to be applied in this order: an abandoned prefix released its
    /// own action, and the new key resolved as well.
    Pair(Action, Action),
    /// The key started a sequence; the next key, or the timeout, decides.
    Pending,
    /// An abandoned prefix released this action, and the new key started a new
    /// sequence.
    ActionThenPending(Action),
}

impl Resolution {
    /// The actions to apply, in order.
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        let (first, second) = match self {
            Self::Unbound | Self::Pending => (None, None),
            Self::Action(action) | Self::ActionThenPending(action) => (Some(action), None),
            Self::Pair(first, second) => (Some(first), Some(second)),
        };
        first.into_iter().chain(second)
    }

    /// Whether a sequence is now waiting for its next key.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending | Self::ActionThenPending(_))
    }

    /// Whether the press produced nothing at all.
    #[must_use]
    pub const fn is_unbound(&self) -> bool {
        matches!(self, Self::Unbound)
    }
}

/// The result of the first half of resolution, before a carried prefix action is
/// folded in. Private so the public [`Resolution`] has no impossible states.
enum Fresh {
    Unbound,
    Action(Action),
    Pending,
}

/// An incomplete multi-key sequence.
#[derive(Clone, Copy, Debug)]
struct PendingSequence {
    mode: InputMode,
    key: KeyPress,
    started_at: Instant,
}

/// Turns key presses into actions, mode-aware and sequence-aware.
///
/// Holds the only mutable input state in the crate — the pending prefix — which
/// is why resolution takes `&mut self` and why the app owns exactly one resolver.
#[derive(Debug)]
pub struct KeyResolver {
    keymap: Keymap,
    timeout: Duration,
    pending: Option<PendingSequence>,
}

impl KeyResolver {
    /// A resolver over `keymap` with the default sequence timeout.
    #[must_use]
    pub const fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            timeout: DEFAULT_SEQUENCE_TIMEOUT,
            pending: None,
        }
    }

    /// A resolver with an explicit sequence timeout.
    #[must_use]
    pub const fn with_timeout(keymap: Keymap, timeout: Duration) -> Self {
        Self {
            keymap,
            timeout,
            pending: None,
        }
    }

    /// The keymap being resolved against, e.g. to generate help.
    #[must_use]
    pub const fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// The sequence timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Whether a sequence is waiting for its next key.
    #[must_use]
    pub const fn has_pending_sequence(&self) -> bool {
        self.pending.is_some()
    }

    /// Forgets any pending sequence.
    ///
    /// The app calls this when the mode changes for a reason other than a key
    /// press, so a half-typed `g` cannot complete against a different mode's
    /// table.
    pub const fn reset(&mut self) {
        self.pending = None;
    }

    /// Resolves one key press in `mode` at monotonic time `now`.
    ///
    /// `now` is an [`Instant`] because sequence timing must not move when the
    /// wall clock does (§8.1).
    #[must_use]
    pub fn resolve(&mut self, mode: InputMode, press: KeyPress, now: Instant) -> Resolution {
        let press = KeyPress::new(press.key, press.modifiers);

        let carried = match self.pending.take() {
            None => None,
            // A mode change invalidates the prefix: the user's context is no
            // longer the one they started typing in.
            Some(pending) if pending.mode != mode => None,
            Some(pending) => {
                let within_window =
                    now.saturating_duration_since(pending.started_at) < self.timeout;
                let completed = if within_window {
                    self.keymap
                        .sequence(mode, pending.key, press)
                        .and_then(|binding| binding.resolved_action(press))
                } else {
                    None
                };
                if let Some(action) = completed {
                    return Resolution::Action(action);
                }
                // The prefix will not be completed, so release whatever it means
                // on its own — `g` cycles the glyph mode (§6.2).
                self.keymap
                    .standalone(mode, pending.key)
                    .and_then(|binding| binding.resolved_action(pending.key))
            }
        };

        match (carried, self.begin(mode, press, now)) {
            (None, Fresh::Unbound) => Resolution::Unbound,
            (None, Fresh::Action(action)) => Resolution::Action(action),
            (None, Fresh::Pending) => Resolution::Pending,
            (Some(carried), Fresh::Unbound) => Resolution::Action(carried),
            (Some(carried), Fresh::Action(action)) => Resolution::Pair(carried, action),
            (Some(carried), Fresh::Pending) => Resolution::ActionThenPending(carried),
        }
    }

    /// Releases a pending sequence whose window has elapsed.
    ///
    /// Returns the prefix's own action, if it has one: after `g` alone times out,
    /// the glyph mode cycles. Call it from the tick handler.
    #[must_use]
    pub fn poll_timeout(&mut self, mode: InputMode, now: Instant) -> Option<Action> {
        let pending = self.pending?;
        if pending.mode != mode {
            self.pending = None;
            return None;
        }
        if now.saturating_duration_since(pending.started_at) < self.timeout {
            return None;
        }
        self.pending = None;
        self.keymap
            .standalone(mode, pending.key)
            .and_then(|binding| binding.resolved_action(pending.key))
    }

    /// Resolves `press` as the start of something new.
    fn begin(&mut self, mode: InputMode, press: KeyPress, now: Instant) -> Fresh {
        // A key that begins a sequence is deferred even when it also has a
        // standalone meaning; that is the `g`/`gg` rule (§6.2).
        if self.keymap.starts_sequence(mode, press) {
            self.pending = Some(PendingSequence {
                mode,
                key: press,
                started_at: now,
            });
            return Fresh::Pending;
        }

        // Exact bindings outrank the printable-character catch-all, so `Ctrl-C`
        // still quits while a filter is being typed.
        if let Some(action) = self
            .keymap
            .standalone(mode, press)
            .and_then(|binding| binding.resolved_action(press))
        {
            return Fresh::Action(action);
        }

        match self
            .keymap
            .typed(mode, press)
            .and_then(|binding| binding.resolved_action(press))
        {
            Some(action) => Fresh::Action(action),
            None => Fresh::Unbound,
        }
    }
}

impl Default for KeyResolver {
    fn default() -> Self {
        Self::new(Keymap::builtin())
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::model::ProcessIdentity;

    use crate::action::{ConfirmationKind, Effect, PendingProcessAction};
    use crate::event::Modifiers;

    use super::*;

    /// A base instant every test measures from.
    fn t0() -> Instant {
        Instant::now()
    }

    fn resolver() -> KeyResolver {
        KeyResolver::default()
    }

    /// Resolves a single press and expects exactly one action.
    fn action_of(mode: InputMode, press: KeyPress) -> Option<Action> {
        let mut resolver = resolver();
        match resolver.resolve(mode, press, t0()) {
            Resolution::Action(action) => Some(action),
            _ => None,
        }
    }

    #[test]
    fn the_builtin_keymap_has_no_conflicts() {
        Keymap::builtin()
            .validate()
            .expect("the default keymap must be conflict-free (§21 M6)");
    }

    #[test]
    fn every_global_binding_from_the_spec_resolves_in_normal_mode() {
        let cases: Vec<(KeyPress, Action)> = vec![
            (KeyPress::char('q'), Action::Quit),
            (KeyPress::ctrl('c'), Action::Quit),
            (KeyPress::char('?'), Action::ToggleHelp),
            (KeyPress::plain(Key::Tab), Action::NextPanel),
            (KeyPress::plain(Key::BackTab), Action::PreviousPanel),
            (KeyPress::char('1'), Action::ChangeView(ViewId::Overview)),
            (KeyPress::char('2'), Action::ChangeView(ViewId::Processes)),
            (KeyPress::char('3'), Action::ChangeView(ViewId::Storage)),
            (KeyPress::char('4'), Action::ChangeView(ViewId::Network)),
            (KeyPress::char('5'), Action::ChangeView(ViewId::Inspect)),
            (KeyPress::char(' '), Action::TogglePause),
            (KeyPress::char('L'), Action::ReturnLive),
            (KeyPress::char(':'), Action::OpenCommandPalette),
            (KeyPress::char('r'), Action::ForceRefresh),
            (KeyPress::char('t'), Action::CycleTheme),
            (KeyPress::plain(Key::Escape), Action::CancelOverlay),
        ];

        for (press, expected) in cases {
            assert_eq!(
                action_of(InputMode::Normal, press),
                Some(expected.clone()),
                "{} should produce {expected:?}",
                press.label()
            );
        }
    }

    #[test]
    fn cycling_the_glyph_mode_is_deferred_in_normal_mode_but_immediate_elsewhere() {
        // §6.2 binds `g` globally and `gg` in lists, so in a list mode `g` waits.
        let mut resolver = resolver();
        assert_eq!(
            resolver.resolve(InputMode::Normal, KeyPress::char('g'), t0()),
            Resolution::Pending
        );

        // The Time Lens has no list, so nothing is ambiguous there.
        assert_eq!(
            action_of(InputMode::TimeLens, KeyPress::char('g')),
            Some(Action::CycleGlyphMode)
        );
    }

    #[test]
    fn every_list_binding_from_the_spec_resolves_in_normal_mode() {
        let cases: Vec<(KeyPress, Action)> = vec![
            (KeyPress::char('j'), Action::SelectNext),
            (KeyPress::plain(Key::Down), Action::SelectNext),
            (KeyPress::char('k'), Action::SelectPrevious),
            (KeyPress::plain(Key::Up), Action::SelectPrevious),
            (KeyPress::ctrl('d'), Action::SelectPageDown),
            (KeyPress::plain(Key::PageDown), Action::SelectPageDown),
            (KeyPress::ctrl('u'), Action::SelectPageUp),
            (KeyPress::plain(Key::PageUp), Action::SelectPageUp),
            (KeyPress::plain(Key::Home), Action::SelectFirst),
            (KeyPress::char('G'), Action::SelectLast),
            (KeyPress::plain(Key::End), Action::SelectLast),
            (KeyPress::plain(Key::Enter), Action::InspectSelected),
            (KeyPress::char('/'), Action::BeginFilterEdit),
            (KeyPress::char('n'), Action::NextMatch),
            (KeyPress::char('N'), Action::PreviousMatch),
            (KeyPress::char('s'), Action::OpenSortSelector),
            (KeyPress::char('S'), Action::ReverseSort),
            (KeyPress::char('p'), Action::PinSelected),
            (KeyPress::char('f'), Action::ToggleTreeView),
        ];

        for (press, expected) in cases {
            assert_eq!(
                action_of(InputMode::Normal, press),
                Some(expected.clone()),
                "{} should produce {expected:?}",
                press.label()
            );
        }
    }

    #[test]
    fn every_process_control_binding_only_proposes() {
        let cases: Vec<(KeyPress, Action)> = vec![
            (KeyPress::char('x'), Action::OpenSignalDialog),
            (KeyPress::char('T'), Action::ProposeSignal(SignalKind::Term)),
            (KeyPress::char('K'), Action::ProposeSignal(SignalKind::Kill)),
            (KeyPress::char('R'), Action::ProposeRenice),
        ];

        for (press, expected) in cases {
            let resolved = action_of(InputMode::Normal, press);
            assert_eq!(resolved, Some(expected.clone()));
            let action = resolved.expect("just asserted");
            assert!(action.is_process_action_proposal());
            assert!(
                !action.can_signal_process(),
                "{expected:?} must not be able to signal (§15.1)"
            );
        }
    }

    #[test]
    fn no_binding_in_any_mode_can_signal_a_process_outside_the_confirmation() {
        let keymap = Keymap::builtin();

        for mode in InputMode::ALL {
            for binding in keymap.bindings_for_mode(mode) {
                if let Some(action) = binding.action()
                    && action.can_signal_process()
                {
                    assert_eq!(
                        mode,
                        InputMode::ConfirmProcessAction,
                        "`{}` can signal in {} mode; §15.1 allows that only from the \
                         confirmation dialog",
                        binding.chord.label(),
                        mode.label()
                    );
                }
            }
        }
    }

    #[test]
    fn no_keypress_can_reach_the_signal_effect_without_a_confirmation() {
        // `Effect::SignalProcess` has exactly one constructor in this crate:
        // `PendingProcessAction::into_effect`, and a `PendingProcessAction` only
        // exists once a proposal has been accepted in the confirmation dialog. So
        // proving the keymap cannot produce a confirmation outside that dialog
        // proves no keypress can reach the effect (§15.1, §17.4).
        let keymap = Keymap::builtin();

        for mode in InputMode::ALL
            .into_iter()
            .filter(|mode| *mode != InputMode::ConfirmProcessAction)
        {
            for binding in keymap.bindings_for_mode(mode) {
                let reachable = binding
                    .action()
                    .is_some_and(|action| action.can_signal_process());
                assert!(
                    !reachable,
                    "`{}` in {} mode could reach Effect::SignalProcess",
                    binding.chord.label(),
                    mode.label()
                );
            }
        }

        // The one legal path, for contrast.
        let pending = PendingProcessAction::Signal {
            identity: ProcessIdentity::new(4_242, 77),
            signal: SignalKind::Kill,
        };
        assert!(matches!(
            pending.into_effect(),
            Effect::SignalProcess { .. }
        ));
        assert!(
            pending
                .confirmation()
                .accepts(&Action::ConfirmForcefulAction)
        );
    }

    #[test]
    fn no_binding_produces_a_signal_request_directly() {
        // `RequestSignal` is the action the reducer turns into
        // `Effect::SignalProcess`. If no binding produces it, no single keypress
        // can reach that effect (§15.1, §17.4).
        for binding in Keymap::builtin().bindings() {
            assert!(
                !matches!(binding.action(), Some(Action::RequestSignal(..))),
                "`{}` requests a signal directly",
                binding.chord.label()
            );
        }
    }

    #[test]
    fn confirmations_are_bound_only_in_the_confirmation_mode() {
        let keymap = Keymap::builtin();

        for binding in keymap.bindings() {
            if matches!(
                binding.action(),
                Some(Action::ConfirmPendingAction | Action::ConfirmForcefulAction)
            ) {
                assert_eq!(
                    binding.modes,
                    CONFIRM_MODE,
                    "`{}` confirms outside the dialog",
                    binding.chord.label()
                );
            }
        }
    }

    #[test]
    fn the_forceful_confirmation_is_distinct_from_enter_and_from_the_kill_key() {
        assert_eq!(
            action_of(InputMode::ConfirmProcessAction, KeyPress::plain(Key::Enter)),
            Some(Action::ConfirmPendingAction)
        );
        assert_eq!(
            action_of(InputMode::ConfirmProcessAction, KeyPress::char('Y')),
            Some(Action::ConfirmForcefulAction)
        );
        assert_eq!(
            action_of(InputMode::ConfirmProcessAction, KeyPress::char('K')),
            None,
            "a second press of K must not confirm a kill"
        );
    }

    #[test]
    fn the_confirmation_hints_name_keys_that_are_actually_bound() {
        // §6.2 requires the dialog to show an explicit confirmation key. If the
        // hint and the keymap ever disagree, the dialog is lying about how to
        // confirm a destructive action.
        let keymap = Keymap::builtin();

        for (hint, expected) in [
            (
                ConfirmationKind::Ordinary.key_hint(),
                Action::ConfirmPendingAction,
            ),
            (
                ConfirmationKind::Forceful.key_hint(),
                Action::ConfirmForcefulAction,
            ),
        ] {
            let bound = keymap
                .bindings_for_mode(InputMode::ConfirmProcessAction)
                .any(|binding| {
                    binding.chord.label() == hint && binding.action() == Some(&expected)
                });
            assert!(
                bound,
                "the dialog promises `{hint}` but no binding produces {expected:?}"
            );
        }
    }

    #[test]
    fn q_types_a_literal_character_while_editing_a_filter() {
        assert_eq!(
            action_of(InputMode::FilterEdit, KeyPress::char('q')),
            Some(Action::InsertChar('q')),
            "§6.2: `q` quits *unless editing text*"
        );
        assert_eq!(
            action_of(InputMode::CommandPalette, KeyPress::char('q')),
            Some(Action::InsertChar('q'))
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::char('q')),
            Some(Action::Quit)
        );
    }

    #[test]
    fn every_printable_key_types_itself_while_editing() {
        for c in ['q', 'Q', ':', ' ', '/', '1', '?', 'ü'] {
            assert_eq!(
                action_of(InputMode::FilterEdit, KeyPress::char(c)),
                Some(Action::InsertChar(c)),
                "{c:?} should be typed literally"
            );
        }
    }

    #[test]
    fn control_chords_keep_their_meaning_while_editing() {
        assert_eq!(
            action_of(InputMode::FilterEdit, KeyPress::ctrl('u')),
            Some(Action::ClearInput)
        );
        assert_eq!(
            action_of(InputMode::FilterEdit, KeyPress::ctrl('w')),
            Some(Action::DeleteWordBackward)
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::ctrl('u')),
            Some(Action::SelectPageUp),
            "the same chord pages up in a list"
        );
    }

    #[test]
    fn ctrl_c_quits_in_every_mode() {
        for mode in InputMode::ALL {
            assert_eq!(
                action_of(mode, KeyPress::ctrl('c')),
                Some(Action::Quit),
                "Ctrl-C must quit in {} mode",
                mode.label()
            );
        }
    }

    #[test]
    fn escape_cancels_in_every_mode() {
        for mode in InputMode::ALL {
            assert_eq!(
                action_of(mode, KeyPress::plain(Key::Escape)),
                Some(Action::CancelOverlay),
                "Esc must cancel in {} mode",
                mode.label()
            );
        }
    }

    #[test]
    fn q_never_quits_from_a_text_entry_mode() {
        let keymap = Keymap::builtin();

        for mode in InputMode::ALL
            .into_iter()
            .filter(|mode| mode.is_text_entry())
        {
            let quits = keymap
                .bindings_for_mode(mode)
                .any(|binding| binding.chord == Chord::key(KeyPress::char('q')));
            assert!(!quits, "`q` must not be bound in {} mode", mode.label());
        }
    }

    #[test]
    fn gg_reaches_the_first_row_when_typed_inside_the_window() {
        let mut resolver = resolver();
        let start = t0();

        assert_eq!(
            resolver.resolve(InputMode::Normal, KeyPress::char('g'), start),
            Resolution::Pending
        );
        assert!(resolver.has_pending_sequence());

        let second = start + Duration::from_millis(120);
        assert_eq!(
            resolver.resolve(InputMode::Normal, KeyPress::char('g'), second),
            Resolution::Action(Action::SelectFirst)
        );
        assert!(!resolver.has_pending_sequence());
    }

    #[test]
    fn a_lone_g_releases_the_glyph_action_once_the_window_elapses() {
        let mut resolver = resolver();
        let start = t0();
        assert_eq!(
            resolver.resolve(InputMode::Normal, KeyPress::char('g'), start),
            Resolution::Pending
        );

        assert_eq!(
            resolver.poll_timeout(InputMode::Normal, start + Duration::from_millis(100)),
            None,
            "inside the window the sequence is still open"
        );
        assert!(resolver.has_pending_sequence());

        assert_eq!(
            resolver.poll_timeout(InputMode::Normal, start + DEFAULT_SEQUENCE_TIMEOUT),
            Some(Action::CycleGlyphMode)
        );
        assert!(!resolver.has_pending_sequence());
        assert_eq!(
            resolver.poll_timeout(InputMode::Normal, start + Duration::from_secs(5)),
            None,
            "the release happens once"
        );
    }

    #[test]
    fn a_second_g_after_the_window_starts_a_new_sequence() {
        let mut resolver = resolver();
        let start = t0();
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::char('g'), start)
                .is_pending()
        );

        let late = start + DEFAULT_SEQUENCE_TIMEOUT + Duration::from_millis(1);
        let resolution = resolver.resolve(InputMode::Normal, KeyPress::char('g'), late);

        assert_eq!(
            resolution,
            Resolution::ActionThenPending(Action::CycleGlyphMode),
            "the first g is too old to complete a sequence, so it acts on its own"
        );
        assert!(resolver.has_pending_sequence());
    }

    #[test]
    fn abandoning_a_prefix_applies_both_actions_in_order() {
        let mut resolver = resolver();
        let start = t0();
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::char('g'), start)
                .is_pending()
        );

        let resolution = resolver.resolve(
            InputMode::Normal,
            KeyPress::char('k'),
            start + Duration::from_millis(50),
        );

        assert_eq!(
            resolution,
            Resolution::Pair(Action::CycleGlyphMode, Action::SelectPrevious)
        );
        assert_eq!(
            resolution.actions().collect::<Vec<_>>(),
            vec![&Action::CycleGlyphMode, &Action::SelectPrevious]
        );
        assert!(!resolver.has_pending_sequence());
    }

    #[test]
    fn a_prefix_with_no_standalone_meaning_is_simply_forgotten() {
        // In Help mode `gg` scrolls to the top, but `g` alone means nothing there.
        let mut resolver = resolver();
        let start = t0();

        assert_eq!(
            resolver.resolve(InputMode::Help, KeyPress::char('g'), start),
            Resolution::Pending
        );
        assert_eq!(
            resolver.poll_timeout(InputMode::Help, start + DEFAULT_SEQUENCE_TIMEOUT),
            None
        );
        assert!(!resolver.has_pending_sequence());

        assert_eq!(
            resolver.resolve(InputMode::Help, KeyPress::char('g'), start),
            Resolution::Pending
        );
        assert_eq!(
            resolver.resolve(
                InputMode::Help,
                KeyPress::char('g'),
                start + Duration::from_millis(10)
            ),
            Resolution::Action(Action::SelectFirst)
        );
    }

    #[test]
    fn changing_mode_discards_a_pending_prefix() {
        let mut resolver = resolver();
        let start = t0();
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::char('g'), start)
                .is_pending()
        );

        let resolution = resolver.resolve(
            InputMode::TimeLens,
            KeyPress::char('g'),
            start + Duration::from_millis(10),
        );

        assert_eq!(
            resolution,
            Resolution::Action(Action::CycleGlyphMode),
            "the prefix belonged to the previous mode and must not leak into this one"
        );
        assert!(!resolver.has_pending_sequence());
    }

    #[test]
    fn reset_forgets_a_pending_prefix() {
        let mut resolver = resolver();
        let start = t0();
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::char('g'), start)
                .is_pending()
        );
        assert!(resolver.has_pending_sequence());

        resolver.reset();

        assert!(!resolver.has_pending_sequence());
        assert_eq!(
            resolver.resolve(
                InputMode::Normal,
                KeyPress::char('g'),
                start + Duration::from_millis(10)
            ),
            Resolution::Pending
        );
    }

    #[test]
    fn poll_timeout_drops_a_prefix_left_behind_by_a_mode_change() {
        let mut resolver = resolver();
        let start = t0();
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::char('g'), start)
                .is_pending()
        );

        assert_eq!(resolver.poll_timeout(InputMode::Help, start), None);
        assert!(!resolver.has_pending_sequence());
    }

    #[test]
    fn home_and_end_seek_in_the_time_lens_and_select_in_a_list() {
        assert_eq!(
            action_of(InputMode::TimeLens, KeyPress::plain(Key::Home)),
            Some(Action::SeekHistory(Seek::Oldest))
        );
        assert_eq!(
            action_of(InputMode::TimeLens, KeyPress::plain(Key::End)),
            Some(Action::SeekHistory(Seek::Newest))
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::plain(Key::Home)),
            Some(Action::SelectFirst)
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::plain(Key::End)),
            Some(Action::SelectLast)
        );
    }

    #[test]
    fn brackets_seek_and_their_shifted_forms_leap() {
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::char('[')),
            Some(Action::SeekHistory(Seek::step_back()))
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::char(']')),
            Some(Action::SeekHistory(Seek::step_forward()))
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::char('{')),
            Some(Action::SeekHistory(Seek::leap_back()))
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::char('}')),
            Some(Action::SeekHistory(Seek::leap_forward()))
        );
    }

    #[test]
    fn arrow_keys_only_seek_once_the_time_lens_has_focus() {
        assert_eq!(
            action_of(InputMode::TimeLens, KeyPress::plain(Key::Left)),
            Some(Action::SeekHistory(Seek::step_back()))
        );
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::plain(Key::Left)),
            None,
            "§2.1 gives the arrows to the Time Lens only"
        );
        assert_eq!(
            action_of(InputMode::FilterEdit, KeyPress::plain(Key::Left)),
            Some(Action::MoveCursorLeft)
        );
    }

    #[test]
    fn help_closes_with_the_same_key_that_opened_it() {
        assert_eq!(
            action_of(InputMode::Normal, KeyPress::char('?')),
            Some(Action::ToggleHelp)
        );
        assert_eq!(
            action_of(InputMode::Help, KeyPress::char('?')),
            Some(Action::CancelOverlay)
        );
    }

    #[test]
    fn unbound_keys_resolve_to_nothing() {
        let mut resolver = resolver();

        assert_eq!(
            resolver.resolve(InputMode::Normal, KeyPress::plain(Key::Function(7)), t0()),
            Resolution::Unbound
        );
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::ctrl('z'), t0())
                .is_unbound()
        );
    }

    #[test]
    fn unnormalized_presses_still_match_their_binding() {
        // A terminal that reports Shift with an upper-case character must not
        // break the binding (§6.2).
        let shifted = KeyPress {
            key: Key::Char('G'),
            modifiers: Modifiers::SHIFT,
        };

        assert_eq!(
            action_of(InputMode::Normal, shifted),
            Some(Action::SelectLast)
        );
    }

    #[test]
    fn generated_help_covers_every_binding_in_the_keymap() {
        let keymap = Keymap::builtin();
        let help: Vec<(InputMode, Vec<HelpSection>)> = InputMode::ALL
            .into_iter()
            .map(|mode| (mode, keymap.help(mode)))
            .collect();

        for binding in keymap.bindings() {
            let label = binding.chord.label();
            let covered = help.iter().any(|(mode, sections)| {
                binding.modes.contains(*mode)
                    && sections.iter().any(|section| {
                        section.title == binding.section.title()
                            && section.entries.iter().any(|entry| {
                                entry.description == binding.description
                                    && entry
                                        .keys
                                        .split(" / ")
                                        .any(|listed| listed == label.as_str())
                            })
                    })
            });
            assert!(
                covered,
                "§7.6: `{label}` ({}) is missing from generated help",
                binding.description
            );
        }
    }

    #[test]
    fn generated_help_is_context_aware() {
        let keymap = Keymap::builtin();

        let normal = keymap.help(InputMode::Normal);
        assert!(
            normal
                .iter()
                .any(|section| section.title == BindingSection::Navigation.title()),
            "the process table has a list"
        );
        assert!(
            normal
                .iter()
                .all(|section| section.title != BindingSection::TextEntry.title()),
            "nothing is being typed in normal mode"
        );

        let filter = keymap.help(InputMode::FilterEdit);
        assert!(
            filter
                .iter()
                .any(|section| section.title == BindingSection::TextEntry.title())
        );
        assert!(
            filter
                .iter()
                .flat_map(|section| &section.entries)
                .all(|entry| entry.description != "Quit"),
            "`q` does not quit while editing, so help must not claim it does"
        );
    }

    #[test]
    fn generated_help_merges_chords_that_share_a_description() {
        let sections = Keymap::builtin().help(InputMode::Normal);
        let navigation = sections
            .iter()
            .find(|section| section.title == BindingSection::Navigation.title())
            .expect("normal mode has navigation help");

        let next_row = navigation
            .entries
            .iter()
            .find(|entry| entry.description == "Next row")
            .expect("`j` and Down share a description");

        assert_eq!(next_row.keys, "j / Down");
    }

    #[test]
    fn help_entries_are_never_empty_and_always_readable() {
        let keymap = Keymap::builtin();

        for mode in InputMode::ALL {
            for section in keymap.help(mode) {
                assert!(!section.entries.is_empty(), "empty section in help");
                for entry in section.entries {
                    assert!(!entry.keys.is_empty());
                    assert!(!entry.description.is_empty());
                    assert!(
                        !entry.description.ends_with('.'),
                        "help text is a clause, not a sentence: {}",
                        entry.description
                    );
                }
            }
        }
    }

    #[test]
    fn a_duplicate_binding_is_rejected_and_names_both_actions() {
        let bindings = vec![
            Binding::new(
                BindingSection::Navigation,
                TABLE_MODE,
                Chord::key(KeyPress::char('j')),
                Action::SelectNext,
                "Next row",
            ),
            Binding::new(
                BindingSection::Navigation,
                NAV_MODES,
                Chord::key(KeyPress::char('j')),
                Action::SelectPrevious,
                "Previous row",
            ),
        ];

        let error =
            Keymap::from_bindings(bindings).expect_err("`j` is claimed twice in normal mode");

        match error {
            KeymapError::Conflict {
                mode,
                key,
                first,
                second,
            } => {
                assert_eq!(mode, "normal");
                assert_eq!(key, "j");
                assert_eq!(first, "SelectNext");
                assert_eq!(second, "SelectPrevious");
            }
            other => panic!("expected a conflict, got {other}"),
        }
    }

    #[test]
    fn a_conflict_message_names_the_key_and_both_actions() {
        let error = KeymapError::Conflict {
            mode: "normal",
            key: "gg".to_owned(),
            first: "SelectFirst".to_owned(),
            second: "Quit".to_owned(),
        };

        let rendered = error.to_string();

        assert!(rendered.contains("gg"), "{rendered}");
        assert!(rendered.contains("SelectFirst"), "{rendered}");
        assert!(rendered.contains("Quit"), "{rendered}");
        assert!(rendered.contains("normal"), "{rendered}");
    }

    #[test]
    fn the_same_key_in_two_modes_is_not_a_conflict() {
        let bindings = vec![
            Binding::new(
                BindingSection::Navigation,
                TABLE_MODE,
                Chord::key(KeyPress::char('n')),
                Action::NextMatch,
                "Next filter match",
            ),
            Binding::new(
                BindingSection::ProcessControl,
                CONFIRM_MODE,
                Chord::key(KeyPress::char('n')),
                Action::CancelOverlay,
                "Cancel without acting",
            ),
        ];

        Keymap::from_bindings(bindings).expect("different modes may reuse a key");
    }

    #[test]
    fn a_prefix_and_its_sequence_are_not_a_conflict() {
        let bindings = vec![
            Binding::new(
                BindingSection::Global,
                TABLE_MODE,
                Chord::key(KeyPress::char('g')),
                Action::CycleGlyphMode,
                "Cycle the glyph mode",
            ),
            Binding::new(
                BindingSection::Navigation,
                TABLE_MODE,
                Chord::pair(KeyPress::char('g'), KeyPress::char('g')),
                Action::SelectFirst,
                "First row",
            ),
        ];

        Keymap::from_bindings(bindings).expect("`g` and `gg` are different chords");
    }

    #[test]
    fn a_binding_with_no_mode_is_rejected() {
        let bindings = vec![Binding::new(
            BindingSection::Global,
            ModeSet::EMPTY,
            Chord::key(KeyPress::char('q')),
            Action::Quit,
            "Quit",
        )];

        let error = Keymap::from_bindings(bindings).expect_err("a binding must apply somewhere");

        assert!(matches!(error, KeymapError::InvalidBinding { .. }));
    }

    #[test]
    fn the_printable_pattern_may_only_insert_the_typed_character() {
        let bindings = vec![Binding::new(
            BindingSection::TextEntry,
            TEXT_MODES,
            Chord::printable(),
            Action::Quit,
            "Quit",
        )];

        let error =
            Keymap::from_bindings(bindings).expect_err("a catch-all cannot mean a fixed action");

        assert!(matches!(
            error,
            KeymapError::InvalidBinding {
                reason: "the printable-key pattern can only insert the typed character",
                ..
            }
        ));
    }

    #[test]
    fn typing_the_character_requires_the_printable_pattern() {
        let bindings = vec![Binding {
            chord: Chord::key(KeyPress::char('a')),
            modes: TEXT_MODES,
            outcome: BindingOutcome::InsertTypedChar,
            section: BindingSection::TextEntry,
            description: "Type into the input",
        }];

        let error = Keymap::from_bindings(bindings).expect_err("mismatched outcome");

        assert!(matches!(
            error,
            KeymapError::InvalidBinding {
                reason: "typing a character requires the single printable-key pattern",
                ..
            }
        ));
    }

    #[test]
    fn a_sequence_cannot_end_in_the_catch_all_pattern() {
        let bindings = vec![Binding::new(
            BindingSection::Navigation,
            TABLE_MODE,
            Chord {
                first: KeyPattern::Exact(KeyPress::char('g')),
                second: Some(KeyPattern::PrintableChar),
            },
            Action::SelectFirst,
            "First row",
        )];

        let error = Keymap::from_bindings(bindings).expect_err("an open-ended sequence is a bug");

        assert!(matches!(
            error,
            KeymapError::InvalidBinding {
                reason: "a sequence cannot end in the printable-key pattern",
                ..
            }
        ));
    }

    #[test]
    fn mode_sets_report_membership_and_iterate_in_order() {
        let set = ModeSet::EMPTY
            .with(InputMode::Normal)
            .with(InputMode::TimeLens);

        assert!(set.contains(InputMode::Normal));
        assert!(!set.contains(InputMode::Help));
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![InputMode::Normal, InputMode::TimeLens]
        );
        assert!(ModeSet::EMPTY.is_empty());
        assert!(!ModeSet::ALL.is_empty());
        assert_eq!(ModeSet::ALL.iter().count(), InputMode::ALL.len());
        assert!(
            !ModeSet::ALL
                .without(InputMode::Normal)
                .contains(InputMode::Normal)
        );
    }

    #[test]
    fn quit_modes_are_exactly_the_non_text_modes() {
        for mode in InputMode::ALL {
            assert_eq!(
                QUIT_MODES.contains(mode),
                !mode.is_text_entry(),
                "{} mode",
                mode.label()
            );
        }
    }

    #[test]
    fn chord_labels_read_the_way_the_spec_writes_them() {
        assert_eq!(Chord::key(KeyPress::ctrl('c')).label(), "Ctrl-C");
        assert_eq!(
            Chord::pair(KeyPress::char('g'), KeyPress::char('g')).label(),
            "gg"
        );
        assert_eq!(
            Chord::key(KeyPress::plain(Key::BackTab)).label(),
            "Shift-Tab"
        );
        assert_eq!(Chord::key(KeyPress::char(' ')).label(), "Space");
        assert_eq!(Chord::printable().label(), "any character");
        assert_eq!(
            Chord::pair(KeyPress::char('g'), KeyPress::plain(Key::Home)).label(),
            "g Home"
        );
    }

    #[test]
    fn overlay_modes_are_the_ones_escape_closes() {
        assert!(InputMode::Help.is_overlay());
        assert!(InputMode::ConfirmProcessAction.is_overlay());
        assert!(InputMode::FilterEdit.is_overlay());
        assert!(!InputMode::Normal.is_overlay());
        assert!(!InputMode::TimeLens.is_overlay());
    }

    #[test]
    fn patterns_match_what_they_claim() {
        assert!(KeyPattern::Exact(KeyPress::char('q')).matches(KeyPress::char('q')));
        assert!(!KeyPattern::Exact(KeyPress::char('q')).matches(KeyPress::char('Q')));
        assert!(KeyPattern::PrintableChar.matches(KeyPress::char('q')));
        assert!(!KeyPattern::PrintableChar.matches(KeyPress::ctrl('q')));
        assert!(!KeyPattern::PrintableChar.matches(KeyPress::plain(Key::Enter)));
    }

    #[test]
    fn resolution_reports_pending_and_actions() {
        assert!(Resolution::Pending.is_pending());
        assert!(Resolution::ActionThenPending(Action::Quit).is_pending());
        assert!(!Resolution::Action(Action::Quit).is_pending());
        assert_eq!(Resolution::Unbound.actions().count(), 0);
        assert_eq!(Resolution::Pending.actions().count(), 0);
        assert_eq!(Resolution::Action(Action::Quit).actions().count(), 1);
        assert_eq!(
            Resolution::ActionThenPending(Action::Quit)
                .actions()
                .count(),
            1
        );
        assert_eq!(
            Resolution::Pair(Action::Quit, Action::SelectNext)
                .actions()
                .count(),
            2
        );
    }

    #[test]
    fn a_custom_timeout_is_respected() {
        let mut resolver = KeyResolver::with_timeout(Keymap::builtin(), Duration::from_millis(50));
        let start = t0();
        assert!(
            resolver
                .resolve(InputMode::Normal, KeyPress::char('g'), start)
                .is_pending()
        );

        assert_eq!(resolver.timeout(), Duration::from_millis(50));
        assert_eq!(
            resolver.poll_timeout(InputMode::Normal, start + Duration::from_millis(60)),
            Some(Action::CycleGlyphMode)
        );
    }

    #[test]
    fn the_resolver_exposes_its_keymap_for_help() {
        let resolver = resolver();

        assert!(!resolver.keymap().help(InputMode::Normal).is_empty());
        assert_eq!(
            resolver.keymap().bindings().len(),
            Keymap::builtin().bindings().len()
        );
    }

    #[test]
    fn direct_binding_lookup_finds_sequences_and_singles() {
        let keymap = Keymap::builtin();

        assert!(
            keymap
                .binding(
                    InputMode::Normal,
                    &Chord::pair(KeyPress::char('g'), KeyPress::char('g'))
                )
                .is_some()
        );
        assert!(
            keymap
                .binding(InputMode::Normal, &Chord::key(KeyPress::char('q')))
                .is_some()
        );
        assert!(
            keymap
                .binding(InputMode::FilterEdit, &Chord::key(KeyPress::char('q')))
                .is_none()
        );
    }
}
