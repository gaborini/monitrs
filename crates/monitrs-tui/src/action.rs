//! Actions and effects: the two halves of the reducer's contract (§10.2, §10.5).
//!
//! An [`Action`] is *what the user asked for*. An [`Effect`] is *what the outside
//! world must be asked to do*. The reducer maps events to actions, applies
//! actions to state, and returns effects; it never performs one. That separation
//! is what makes the §15.1 safety dialogs testable without signalling a real
//! process, and it is why every keyboard-reachable process action in this module
//! is a *proposal* rather than a request.
//!
//! # State mutation is crate-private (§6.1)
//!
//! Widgets take `&State` and render. They cannot mutate state or send signals
//! because the only way to change anything is to hand an [`Action`] to the
//! reducer, and the reducer lives on the main thread with the only `&mut State`
//! in the process. Nothing in this module exposes a mutator, and the app state
//! module keeps its `&mut` API crate-private for the same reason.
//!
//! # Additions to the §10.2/§10.5 lists
//!
//! The spec's enums are the required core, not the whole surface: §6.2 binds
//! keys that they do not cover. Every addition here exists because a §6.2
//! binding or a §6.3 palette command would otherwise be unimplementable, and
//! each is annotated with the clause that demands it. Two are worth calling out:
//!
//! * [`Effect::RequestSample`] — §6.2 binds `r` to *force refresh*, which is a
//!   request to the sampler thread and therefore an effect, not a state change.
//! * [`Effect::ReniceProcess`] — §6.2 binds `R` to *propose renice dialog where
//!   supported*. A confirmation dialog that can never execute would be a
//!   placeholder, so the effect exists; whether a platform supports it is the
//!   collector's answer, not the reducer's.

use std::path::PathBuf;

use monitrs_core::model::ProcessIdentity;

/// The seven top-level views, bound to `1`–`7` by §6.2.
///
/// `Cpu` sits next to `Processes` on purpose: "which process is busy" and "which core
/// is busy" are the same question asked from two ends, and a user who has just sorted
/// the table by CPU is one key away from the cores. It cost renumbering the three
/// screens after it, which is a real price paid once.
///
/// `Battery` goes last for the opposite reason. On most machines it is the screen with
/// the least to say — a server has no battery at all, and says so — so a screen that
/// reads `n/a` on half the hosts monitrs runs on has no claim on a low digit. Putting
/// it at the end also renumbered nothing, which is why it is there rather than beside
/// the sensors it shares a screen with.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ViewId {
    /// §7.1: CPU, memory, load, pressure radar, top processes.
    Overview,
    /// §7.2: the process table.
    Processes,
    /// Per-core utilization, the aggregate breakdown, and core topology.
    Cpu,
    /// §7.3: filesystems and devices.
    Storage,
    /// §7.4: interfaces and throughput.
    Network,
    /// §7.5: the detailed explanation surface.
    Inspect,
    /// Battery charge, wear, and power draw, plus the thermal sensors.
    Battery,
}

impl ViewId {
    /// Every view, in `1`–`7` order.
    pub const ALL: [Self; 7] = [
        Self::Overview,
        Self::Processes,
        Self::Cpu,
        Self::Storage,
        Self::Network,
        Self::Inspect,
        Self::Battery,
    ];

    /// The digit key §6.2 binds this view to.
    #[must_use]
    pub const fn digit(self) -> char {
        match self {
            Self::Overview => '1',
            Self::Processes => '2',
            Self::Cpu => '3',
            Self::Storage => '4',
            Self::Network => '5',
            Self::Inspect => '6',
            Self::Battery => '7',
        }
    }

    /// The view a digit selects, if any.
    #[must_use]
    pub const fn from_digit(digit: char) -> Option<Self> {
        match digit {
            '1' => Some(Self::Overview),
            '2' => Some(Self::Processes),
            '3' => Some(Self::Cpu),
            '4' => Some(Self::Storage),
            '5' => Some(Self::Network),
            '6' => Some(Self::Inspect),
            '7' => Some(Self::Battery),
            _ => None,
        }
    }

    /// The title shown in the view tab bar.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Processes => "Processes",
            Self::Cpu => "CPU",
            Self::Storage => "Storage",
            Self::Network => "Network",
            Self::Inspect => "Inspect",
            Self::Battery => "Battery",
        }
    }

    /// The token used by the `view <name>` palette command (§6.3).
    #[must_use]
    pub const fn palette_token(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Processes => "processes",
            Self::Cpu => "cpu",
            Self::Storage => "storage",
            Self::Network => "network",
            Self::Inspect => "inspect",
            Self::Battery => "battery",
        }
    }

    /// Parses a `view <name>` argument. Case-insensitive, exact otherwise:
    /// §6.3 requires deterministic, locally implemented parsing.
    #[must_use]
    pub fn from_palette_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|view| view.palette_token().eq_ignore_ascii_case(token))
    }
}

/// A movement through the retained history (§2.1).
///
/// Sample counts rather than durations, because the real interval varies (§8.1)
/// and the user is stepping through *samples*, not through wall-clock time.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Seek {
    /// Move this many samples toward the past.
    Backward(u32),
    /// Move this many samples toward the present.
    Forward(u32),
    /// Jump to the oldest retained sample.
    Oldest,
    /// Jump to the newest retained sample, which is not the same as going live:
    /// §2.1 makes returning to live one explicit action, `L`.
    Newest,
}

impl Seek {
    /// One sample: the `[` and `]` bindings.
    pub const STEP: u32 = 1;

    /// Ten samples: the `{` and `}` bindings, i.e. the ×10 variants.
    pub const LEAP: u32 = 10;

    /// One sample back.
    #[must_use]
    pub const fn step_back() -> Self {
        Self::Backward(Self::STEP)
    }

    /// One sample forward.
    #[must_use]
    pub const fn step_forward() -> Self {
        Self::Forward(Self::STEP)
    }

    /// Ten samples back.
    #[must_use]
    pub const fn leap_back() -> Self {
        Self::Backward(Self::LEAP)
    }

    /// Ten samples forward.
    #[must_use]
    pub const fn leap_forward() -> Self {
        Self::Forward(Self::LEAP)
    }

    /// Whether this seek asks for no movement, which the reducer must treat as a
    /// no-op rather than as a request to re-render.
    #[must_use]
    pub const fn is_noop(self) -> bool {
        matches!(self, Self::Backward(0) | Self::Forward(0))
    }
}

/// The signals §9.2 puts in the default confirmation dialog.
///
/// The numbers are the POSIX values, identical on Linux and macOS. Only the
/// collector crate turns one into an actual signal; this is a request, and the
/// keymap can never produce one without a confirmation (§15.1).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SignalKind {
    /// `SIGTERM`: ask the process to exit.
    Term,
    /// `SIGINT`: what `Ctrl-C` in a shell would send.
    Int,
    /// `SIGHUP`: hang-up, which many daemons read as "reload".
    Hup,
    /// `SIGKILL`: unstoppable. Last in the dialog and marked forceful (§9.2).
    Kill,
}

impl SignalKind {
    /// The dialog order §9.2 mandates: `SIGKILL` last, so it is never the
    /// default and never adjacent to a habitual first choice.
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
    pub const fn number(self) -> i32 {
        match self {
            Self::Hup => 1,
            Self::Int => 2,
            Self::Kill => 9,
            Self::Term => 15,
        }
    }

    /// Whether the process cannot refuse or clean up after this signal.
    ///
    /// Drives the visual marking §9.2 asks for and the distinct confirmation key
    /// §15.1 asks for.
    #[must_use]
    pub const fn is_forceful(self) -> bool {
        matches!(self, Self::Kill)
    }

    /// The consequence sentence the confirmation dialog must show (§6.2).
    #[must_use]
    pub const fn consequence(self) -> &'static str {
        match self {
            Self::Term => "asks the process to exit; it may clean up first, or ignore the request",
            Self::Int => "interrupts the process, as Ctrl-C in its own terminal would",
            Self::Hup => "reports a hang-up; many services reload their configuration instead",
            Self::Kill => "terminates the process immediately, with no cleanup and no unsaved work",
        }
    }
}

/// A sortable process column (§7.2).
///
/// Local to this crate on purpose: `monitrs_core::process::ProcessSort` is being
/// written in parallel and will own the comparator and the stable tie-breaker.
/// This enum only names the column a key or palette command selected.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SortField {
    /// Process identifier.
    Pid,
    /// Owning user.
    User,
    /// Process state.
    State,
    /// CPU percentage.
    Cpu,
    /// Resident memory as a share of total.
    MemoryShare,
    /// Resident set size in bytes.
    Rss,
    /// Virtual size in bytes.
    VirtualMemory,
    /// Read throughput.
    ReadRate,
    /// Write throughput.
    WriteRate,
    /// Thread count.
    Threads,
    /// Time since the process started.
    Age,
    /// Process name.
    Name,
    /// Full command line.
    Command,
}

impl SortField {
    /// Every field, in §7.2 column order.
    pub const ALL: [Self; 13] = [
        Self::Pid,
        Self::User,
        Self::State,
        Self::Cpu,
        Self::MemoryShare,
        Self::Rss,
        Self::VirtualMemory,
        Self::ReadRate,
        Self::WriteRate,
        Self::Threads,
        Self::Age,
        Self::Name,
        Self::Command,
    ];

    /// The canonical token used by `sort <field>` (§6.3) and by the config file
    /// key `processes.sort` (§12).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::User => "user",
            Self::State => "state",
            Self::Cpu => "cpu",
            Self::MemoryShare => "memory",
            Self::Rss => "rss",
            Self::VirtualMemory => "virtual",
            Self::ReadRate => "read",
            Self::WriteRate => "write",
            Self::Threads => "threads",
            Self::Age => "age",
            Self::Name => "name",
            Self::Command => "command",
        }
    }

    /// The column header shown in the process table.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pid => "PID",
            Self::User => "USER",
            Self::State => "S",
            Self::Cpu => "CPU%",
            Self::MemoryShare => "MEM%",
            Self::Rss => "RSS",
            Self::VirtualMemory => "VIRT",
            Self::ReadRate => "READ",
            Self::WriteRate => "WRITE",
            Self::Threads => "THR",
            Self::Age => "AGE",
            Self::Name => "NAME",
            Self::Command => "COMMAND",
        }
    }

    /// Parses a `sort <field>` argument.
    ///
    /// Accepts the canonical token plus the aliases a user of another monitor
    /// would try. Case-insensitive, but never a prefix match: §6.3 requires
    /// deterministic parsing, and `s` must not silently mean `state` today and
    /// something else after a column is added.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        let lowered = token.trim().to_ascii_lowercase();
        match lowered.as_str() {
            "pid" => Some(Self::Pid),
            "user" | "owner" => Some(Self::User),
            "state" | "status" => Some(Self::State),
            "cpu" | "cpu%" => Some(Self::Cpu),
            "memory" | "mem" | "mem%" | "memory%" => Some(Self::MemoryShare),
            "rss" | "resident" => Some(Self::Rss),
            "virtual" | "virt" => Some(Self::VirtualMemory),
            "read" | "read-rate" => Some(Self::ReadRate),
            "write" | "write-rate" => Some(Self::WriteRate),
            "threads" | "thread" => Some(Self::Threads),
            "age" | "started" => Some(Self::Age),
            "name" => Some(Self::Name),
            "command" | "cmd" => Some(Self::Command),
            _ => None,
        }
    }

    /// Whether selecting this field should start descending.
    ///
    /// Consumption columns are interesting from the top down; text columns read
    /// as a list, so they start ascending (§7.2).
    #[must_use]
    pub const fn defaults_descending(self) -> bool {
        match self {
            Self::Cpu
            | Self::MemoryShare
            | Self::Rss
            | Self::VirtualMemory
            | Self::ReadRate
            | Self::WriteRate
            | Self::Threads
            | Self::Age => true,
            Self::Pid | Self::User | Self::State | Self::Name | Self::Command => false,
        }
    }
}

/// Everything the reducer can be asked to do (§10.2).
///
/// The variants the spec lists are all here; the rest exist because §6.2 or §6.3
/// binds them. Two shapes recur:
///
/// * **Selection-relative vs identity-bearing.** A keypress cannot know which
///   process is selected, so `p` produces [`Action::PinSelected`] and the reducer
///   resolves it to [`Action::Pin`] with a [`ProcessIdentity`]. The palette and
///   the reducer produce the identity-bearing forms directly. §26: a PID is not
///   an identity, so the identity travels with the action from then on.
/// * **Proposal vs request.** [`Action::ProposeSignal`] opens a dialog;
///   [`Action::RequestSignal`] executes one. No key produces the latter (§15.1),
///   which [`Action::can_signal_process`] lets the keymap test assert.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    // ---- application ----
    /// Leave, restoring the terminal (§14.3).
    Quit,
    /// Ask the sampler for a sample now (§6.2 `r`).
    ForceRefresh,
    /// Next theme (§6.2 `t`).
    CycleTheme,
    /// Next glyph mode (§6.2 `g`, §5.1).
    CycleGlyphMode,
    /// Show or hide the generated help overlay (§6.2 `?`, §7.6).
    ToggleHelp,
    /// Open the command palette (§6.2 `:`, §6.3).
    OpenCommandPalette,
    /// Focus the next panel of the current view (§6.2 `Tab`).
    NextPanel,
    /// Focus the previous panel (§6.2 `Shift-Tab`).
    PreviousPanel,
    /// Switch view (§6.2 `1`–`5`).
    ChangeView(ViewId),

    // ---- time lens (§2.1) ----
    /// Freeze or unfreeze the visible timeline without stopping collection.
    TogglePause,
    /// Move through retained history.
    SeekHistory(Seek),
    /// Return to the live sample. The one explicit action §2.1 requires.
    ReturnLive,

    // ---- selection (§6.2 lists and tables) ----
    /// Next row.
    SelectNext,
    /// Previous row.
    SelectPrevious,
    /// One page down.
    SelectPageDown,
    /// One page up.
    SelectPageUp,
    /// First row.
    SelectFirst,
    /// Last row.
    SelectLast,
    /// Inspect the selected item, which for a process means requesting its
    /// detail (§7.5).
    InspectSelected,

    // ---- filtering and sorting ----
    /// Enter `FilterEdit` mode (§6.2 `/`).
    BeginFilterEdit,
    /// Replace the active filter. Produced by submitting the filter editor or by
    /// `filter <text>` (§6.3).
    SetFilter(String),
    /// Move to the next filter match (§6.2 `n`).
    NextMatch,
    /// Move to the previous filter match (§6.2 `N`).
    PreviousMatch,
    /// Open the sort selector (§6.2 `s`).
    OpenSortSelector,
    /// Sort by a column (§6.2's sort selector, §6.3 `sort <field>`).
    SetSort(SortField),
    /// Reverse the current sort direction (§6.2 `S`).
    ReverseSort,
    /// Switch between flat and tree process views (§6.2 `f`).
    ToggleTreeView,

    // ---- pinning (§2.5) ----
    /// Pin or unpin whatever is selected (§6.2 `p`).
    PinSelected,
    /// Pin or unpin a specific process.
    Pin(ProcessIdentity),
    /// Scope the process table to the selected process and its descendants (§6.2 `F`).
    ///
    /// A toggle: following the process already being followed stops following it, so one
    /// key both enters and leaves the scope, as `p` does for pins.
    FollowSelected,
    /// Lift the subtree scope, whatever it was.
    ///
    /// Separate from the toggle because `unfollow` in the palette (§6.3) has to be
    /// unambiguous about which way it goes, and because the reducer needs to lift the
    /// scope itself when the root exits.
    StopFollowing,

    // ---- detail (§7.5) ----
    /// Load the detail of a specific process.
    RequestProcessDetail(ProcessIdentity),

    // ---- process control (§6.2, §15.1) ----
    /// Open the signal dialog for the selected process (§6.2 `x`).
    OpenSignalDialog,
    /// Propose one signal for the selected process (§6.2 `T`, `K`).
    ///
    /// A proposal, never a delivery: it opens `ConfirmProcessAction` mode.
    ProposeSignal(SignalKind),
    /// Propose a renice for the selected process (§6.2 `R`).
    ProposeRenice,
    /// Deliver a signal to a revalidated identity.
    ///
    /// Reachable only from a confirmed [`PendingProcessAction`]. No keymap entry
    /// produces it (§15.1).
    RequestSignal(ProcessIdentity, SignalKind),
    /// Accept the pending destructive action.
    ConfirmPendingAction,
    /// Accept a *forceful* pending action.
    ///
    /// §15.1 wants `SIGKILL` confirmed by something other than ordinary Enter,
    /// so a forceful proposal accepts only this.
    ConfirmForcefulAction,
    /// Close the topmost overlay, or leave the current mode (§6.2 `Esc`).
    CancelOverlay,

    // ---- text entry (§6.1 FilterEdit, CommandPalette) ----
    /// Insert the typed character. This is why `q` types a `q` while editing.
    InsertChar(char),
    /// Delete the character before the cursor.
    DeleteBackward,
    /// Delete the character under the cursor.
    DeleteForward,
    /// Delete the word before the cursor.
    DeleteWordBackward,
    /// Clear the whole input.
    ClearInput,
    /// Move the cursor one character left.
    MoveCursorLeft,
    /// Move the cursor one character right.
    MoveCursorRight,
    /// Move the cursor to the start of the input.
    MoveCursorToStart,
    /// Move the cursor to the end of the input.
    MoveCursorToEnd,
    /// Submit the input: apply the filter, or run the palette command.
    SubmitInput,
}

impl Action {
    /// Whether applying this action can end in [`Effect::SignalProcess`].
    ///
    /// The keymap test uses this to assert that no keypress outside
    /// `ConfirmProcessAction` mode can reach a signal (§15.1). Proposals are
    /// deliberately *not* included: they open a dialog and nothing else.
    #[must_use]
    pub const fn can_signal_process(&self) -> bool {
        matches!(
            self,
            Self::RequestSignal(..) | Self::ConfirmPendingAction | Self::ConfirmForcefulAction
        )
    }

    /// Whether this action only opens a confirmation dialog.
    #[must_use]
    pub const fn is_process_action_proposal(&self) -> bool {
        matches!(
            self,
            Self::OpenSignalDialog | Self::ProposeSignal(_) | Self::ProposeRenice
        )
    }

    /// Whether §15.1 forbids this action while historical data is displayed.
    ///
    /// "Process actions must be unavailable in history" (§26): the identity on
    /// screen is a record of the past and may no longer exist, so acting on it
    /// would act on whatever holds that PID now.
    #[must_use]
    pub const fn is_blocked_in_history(&self) -> bool {
        self.is_process_action_proposal() || self.can_signal_process()
    }

    /// Whether this action edits a text buffer, and therefore only makes sense in
    /// a text-entry mode.
    #[must_use]
    pub const fn is_text_editing(&self) -> bool {
        matches!(
            self,
            Self::InsertChar(_)
                | Self::DeleteBackward
                | Self::DeleteForward
                | Self::DeleteWordBackward
                | Self::ClearInput
                | Self::MoveCursorLeft
                | Self::MoveCursorRight
                | Self::MoveCursorToStart
                | Self::MoveCursorToEnd
                | Self::SubmitInput
        )
    }
}

/// A destructive action awaiting confirmation (§15.1).
///
/// Holding the [`ProcessIdentity`] rather than a PID is what lets the executor
/// detect reuse: §6.2 requires the identity to be re-read immediately before the
/// action runs and the action aborted if the start time changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingProcessAction {
    /// Send a signal.
    Signal {
        /// Who to signal.
        identity: ProcessIdentity,
        /// What to send.
        signal: SignalKind,
    },
    /// Change scheduling priority (§6.2 `R`, where supported).
    Renice {
        /// Whose priority to change.
        identity: ProcessIdentity,
        /// The requested nice value. POSIX range is `-20..=19`; lowering it needs
        /// privileges the app must not escalate to (§15.1).
        nice: i8,
    },
}

impl PendingProcessAction {
    /// The identity the executor must revalidate.
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        match self {
            Self::Signal { identity, .. } | Self::Renice { identity, .. } => *identity,
        }
    }

    /// Whether this action cannot be refused or cleaned up after.
    #[must_use]
    pub const fn is_forceful(&self) -> bool {
        match self {
            Self::Signal { signal, .. } => signal.is_forceful(),
            Self::Renice { .. } => false,
        }
    }

    /// Which confirmation this action demands (§15.1).
    #[must_use]
    pub const fn confirmation(&self) -> ConfirmationKind {
        if self.is_forceful() {
            ConfirmationKind::Forceful
        } else {
            ConfirmationKind::Ordinary
        }
    }

    /// The consequence sentence the dialog must show (§6.2).
    #[must_use]
    pub const fn consequence(&self) -> &'static str {
        match self {
            Self::Signal { signal, .. } => signal.consequence(),
            Self::Renice { .. } => {
                "changes scheduling priority; raising it again may require privileges"
            }
        }
    }

    /// The effect that executes this action once it has been confirmed.
    ///
    /// The only route from a keypress to [`Effect::SignalProcess`], and it starts
    /// at a value that can only exist because a proposal was accepted (§10.5).
    #[must_use]
    pub const fn into_effect(self) -> Effect {
        match self {
            Self::Signal { identity, signal } => Effect::SignalProcess { identity, signal },
            Self::Renice { identity, nice } => Effect::ReniceProcess { identity, nice },
        }
    }
}

/// How strong a confirmation a pending action demands (§15.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationKind {
    /// Enter, or `y`, is enough.
    Ordinary,
    /// Needs the distinct forceful key; Enter must not be enough.
    Forceful,
}

impl ConfirmationKind {
    /// Whether `action` is an acceptable confirmation.
    ///
    /// A forceful action accepts only [`Action::ConfirmForcefulAction`]. An
    /// ordinary one accepts either, so a user who reaches for the stronger key
    /// is never told off for being careful.
    #[must_use]
    pub const fn accepts(self, action: &Action) -> bool {
        match self {
            Self::Ordinary => matches!(
                action,
                Action::ConfirmPendingAction | Action::ConfirmForcefulAction
            ),
            Self::Forceful => matches!(action, Action::ConfirmForcefulAction),
        }
    }

    /// The key hint the dialog prints next to its confirm button (§6.2 requires
    /// an explicit confirmation key to be shown).
    ///
    /// Written the way §6.2 writes keys — a bare capital letter means the shifted
    /// key, as in `G` or `K` — and pinned to the real keymap by
    /// `the_confirmation_hints_name_keys_that_are_actually_bound` in
    /// [`crate::keymap`].
    #[must_use]
    pub const fn key_hint(self) -> &'static str {
        match self {
            Self::Ordinary => "Enter",
            Self::Forceful => "Y",
        }
    }
}

/// Something the outside world must do (§10.5).
///
/// The reducer returns these and performs none of them, which is what makes
/// keyboard behaviour and safety dialogs testable without touching a real
/// process, file or terminal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Nothing. Kept because §10.5 lists it; [`Effects::push`] discards it.
    None,
    /// The screen no longer matches the state.
    RequestRedraw,
    /// Ask the sampler for a sample now (§6.2 `r`). Not in §10.5's list; a force
    /// refresh is a request to another thread, so it cannot be a state change.
    RequestSample,
    /// Load a process detail on the on-demand worker (§8.6, §10.3).
    FetchProcessDetail(ProcessIdentity),
    /// Tell the sampler whether a screen showing a sensor reading is visible
    /// (§8.6's on-demand tier).
    ///
    /// Not a state change, so it cannot live in the reducer: the sampler owns its
    /// own clock on another thread, and this is how the visible screen reaches it.
    /// A level, not a pulse — the sampler holds the last value it was told.
    SetSensorInterest(bool),
    /// Send a signal.
    ///
    /// The executor must re-read the identity immediately before delivery and
    /// abort if the start key changed (§6.2, §15.1). It must not escalate with
    /// `sudo`, and must not signal a group or tree (§3.2).
    SignalProcess {
        /// Who to signal, revalidated at the point of delivery.
        identity: ProcessIdentity,
        /// What to send.
        signal: SignalKind,
    },
    /// Change a process's scheduling priority (§6.2 `R`). Not in §10.5's list;
    /// see the module note. Same revalidation rule as [`Effect::SignalProcess`].
    ReniceProcess {
        /// Whose priority to change.
        identity: ProcessIdentity,
        /// The requested nice value.
        nice: i8,
    },
    /// Ring the terminal bell once (`\x07`).
    ///
    /// Not in §10.5's list. It exists because §10.5's rule is absolute — the
    /// reducer performs no I/O — and a bell is a byte written to the terminal, so
    /// the *decision* to alert can live with the state that detected the
    /// escalation while the write stays outside. Emitted only for an escalation
    /// into `critical`, and only when §12's `diagnostics.bell_on_critical` asks
    /// for it.
    RingBell,
    /// Re-read the configuration file (§6.3 `reload config`). The loader must
    /// validate the whole candidate before replacing anything (§12).
    ReloadConfig,
    /// Write a JSON snapshot (§6.3 `export snapshot <path>`), with the
    /// redactions §15.2 requires.
    ExportSnapshot(PathBuf),
    /// Stop the worker threads, join them, restore the terminal, exit (§10.3).
    Shutdown,
}

impl Effect {
    /// Whether this effect acts on a process rather than on monitrs itself.
    #[must_use]
    pub const fn touches_a_process(&self) -> bool {
        matches!(
            self,
            Self::SignalProcess { .. } | Self::ReniceProcess { .. }
        )
    }

    /// Whether the executor must re-read the process identity before running
    /// this effect (§6.2, §15.1).
    #[must_use]
    pub const fn requires_identity_revalidation(&self) -> bool {
        self.touches_a_process()
    }
}

/// The effects produced by one reduction, in order.
///
/// A reducer step can legitimately produce several — confirming a signal both
/// signals and redraws — and §10.3 forbids unbounded accumulation anywhere, so
/// the list is short-lived and drained by the caller on the same turn.
///
/// [`Effect::None`] is dropped and [`Effect::RequestRedraw`] is deduplicated:
/// requesting a redraw twice in one turn is one redraw, and letting callers push
/// freely is what keeps reducer code readable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Effects(Vec<Effect>);

impl Effects {
    /// An empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// A list holding one effect.
    #[must_use]
    pub fn one(effect: Effect) -> Self {
        let mut effects = Self::new();
        effects.push(effect);
        effects
    }

    /// Adds an effect, dropping [`Effect::None`] and duplicate redraw requests.
    pub fn push(&mut self, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::RequestRedraw if self.0.contains(&Effect::RequestRedraw) => {}
            other => self.0.push(other),
        }
    }

    /// The effects, in the order they were produced.
    #[must_use]
    pub fn as_slice(&self) -> &[Effect] {
        &self.0
    }

    /// Whether nothing needs to happen.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many effects there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether a specific effect is present.
    #[must_use]
    pub fn contains(&self, effect: &Effect) -> bool {
        self.0.contains(effect)
    }

    /// Whether any effect acts on a process, which the history guard checks
    /// before anything is executed (§15.1).
    #[must_use]
    pub fn touches_a_process(&self) -> bool {
        self.0.iter().any(Effect::touches_a_process)
    }

    /// Borrowing iterator.
    pub fn iter(&self) -> core::slice::Iter<'_, Effect> {
        self.0.iter()
    }
}

impl IntoIterator for Effects {
    type Item = Effect;
    type IntoIter = std::vec::IntoIter<Effect>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Effects {
    type Item = &'a Effect;
    type IntoIter = core::slice::Iter<'a, Effect>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(31_842, 900_100)
    }

    #[test]
    fn every_view_round_trips_through_its_digit_key() {
        for view in ViewId::ALL {
            assert_eq!(ViewId::from_digit(view.digit()), Some(view));
            assert!(!view.title().is_empty());
        }
        assert_eq!(
            ViewId::ALL.map(ViewId::digit),
            ['1', '2', '3', '4', '5', '6', '7'],
            "§6.2 binds the views to 1-7 in this order, with CPU inserted at 3 and \
             Battery appended at 7"
        );
        assert_eq!(ViewId::from_digit('8'), None);
        assert_eq!(ViewId::from_digit('0'), None);
    }

    #[test]
    fn every_view_round_trips_through_its_palette_token() {
        for view in ViewId::ALL {
            assert_eq!(ViewId::from_palette_token(view.palette_token()), Some(view));
        }
        assert_eq!(
            ViewId::from_palette_token("PROCESSES"),
            Some(ViewId::Processes),
            "§6.3 tokens are case-insensitive"
        );
        assert_eq!(ViewId::from_palette_token("proc"), None);
    }

    #[test]
    fn sigkill_is_last_in_the_dialog_and_the_only_forceful_signal() {
        assert_eq!(
            SignalKind::DIALOG_ORDER.last(),
            Some(&SignalKind::Kill),
            "§9.2 puts SIGKILL last"
        );
        let forceful: Vec<_> = SignalKind::DIALOG_ORDER
            .into_iter()
            .filter(|signal| signal.is_forceful())
            .collect();
        assert_eq!(forceful, vec![SignalKind::Kill]);
    }

    #[test]
    fn signal_numbers_and_names_match_posix() {
        assert_eq!(SignalKind::Hup.number(), 1);
        assert_eq!(SignalKind::Int.number(), 2);
        assert_eq!(SignalKind::Kill.number(), 9);
        assert_eq!(SignalKind::Term.number(), 15);
        assert_eq!(SignalKind::Term.name(), "SIGTERM");
        for signal in SignalKind::DIALOG_ORDER {
            assert!(!signal.consequence().is_empty());
        }
    }

    #[test]
    fn only_a_confirmed_pending_action_becomes_a_signal_effect() {
        let pending = PendingProcessAction::Signal {
            identity: identity(),
            signal: SignalKind::Term,
        };

        assert_eq!(
            pending.into_effect(),
            Effect::SignalProcess {
                identity: identity(),
                signal: SignalKind::Term,
            }
        );
        assert!(pending.into_effect().requires_identity_revalidation());
    }

    #[test]
    fn a_forceful_pending_action_refuses_an_ordinary_confirmation() {
        let kill = PendingProcessAction::Signal {
            identity: identity(),
            signal: SignalKind::Kill,
        };

        assert_eq!(kill.confirmation(), ConfirmationKind::Forceful);
        assert!(
            !kill.confirmation().accepts(&Action::ConfirmPendingAction),
            "§15.1: SIGKILL must not be confirmed by ordinary Enter"
        );
        assert!(kill.confirmation().accepts(&Action::ConfirmForcefulAction));
    }

    #[test]
    fn an_ordinary_pending_action_accepts_either_confirmation() {
        let term = PendingProcessAction::Signal {
            identity: identity(),
            signal: SignalKind::Term,
        };

        assert_eq!(term.confirmation(), ConfirmationKind::Ordinary);
        assert!(term.confirmation().accepts(&Action::ConfirmPendingAction));
        assert!(term.confirmation().accepts(&Action::ConfirmForcefulAction));
        assert!(!term.confirmation().accepts(&Action::CancelOverlay));
    }

    #[test]
    fn renice_is_never_forceful_but_still_needs_revalidation() {
        let renice = PendingProcessAction::Renice {
            identity: identity(),
            nice: 5,
        };

        assert!(!renice.is_forceful());
        assert_eq!(renice.confirmation(), ConfirmationKind::Ordinary);
        assert!(renice.into_effect().requires_identity_revalidation());
        assert_eq!(renice.identity(), identity());
    }

    #[test]
    fn proposals_cannot_signal_and_confirmations_can() {
        for proposal in [
            Action::OpenSignalDialog,
            Action::ProposeSignal(SignalKind::Kill),
            Action::ProposeRenice,
        ] {
            assert!(proposal.is_process_action_proposal());
            assert!(
                !proposal.can_signal_process(),
                "{proposal:?} must only open a dialog (§15.1)"
            );
        }

        assert!(Action::ConfirmPendingAction.can_signal_process());
        assert!(Action::ConfirmForcefulAction.can_signal_process());
        assert!(Action::RequestSignal(identity(), SignalKind::Term).can_signal_process());
    }

    #[test]
    fn every_process_action_is_blocked_in_history() {
        for action in [
            Action::OpenSignalDialog,
            Action::ProposeSignal(SignalKind::Term),
            Action::ProposeRenice,
            Action::RequestSignal(identity(), SignalKind::Kill),
            Action::ConfirmPendingAction,
            Action::ConfirmForcefulAction,
        ] {
            assert!(
                action.is_blocked_in_history(),
                "§26: {action:?} must be unavailable in history"
            );
        }

        for action in [
            Action::SelectNext,
            Action::ChangeView(ViewId::Inspect),
            Action::SeekHistory(Seek::step_back()),
            Action::ReturnLive,
            Action::Quit,
        ] {
            assert!(!action.is_blocked_in_history());
        }
    }

    #[test]
    fn seek_step_and_leap_differ_by_a_factor_of_ten() {
        assert_eq!(Seek::step_back(), Seek::Backward(1));
        assert_eq!(Seek::leap_back(), Seek::Backward(10));
        assert_eq!(Seek::step_forward(), Seek::Forward(1));
        assert_eq!(Seek::leap_forward(), Seek::Forward(10));
        assert_eq!(Seek::LEAP, Seek::STEP * 10);
        assert!(Seek::Backward(0).is_noop());
        assert!(Seek::Forward(0).is_noop());
        assert!(!Seek::Oldest.is_noop());
        assert!(!Seek::step_back().is_noop());
    }

    #[test]
    fn every_sort_field_round_trips_through_its_token() {
        for field in SortField::ALL {
            assert_eq!(SortField::from_token(field.token()), Some(field));
            assert!(!field.label().is_empty());
        }
    }

    #[test]
    fn the_palette_sort_vocabulary_from_the_spec_parses() {
        // §6.3: sort cpu|memory|read|write|pid|name|age
        assert_eq!(SortField::from_token("cpu"), Some(SortField::Cpu));
        assert_eq!(
            SortField::from_token("memory"),
            Some(SortField::MemoryShare)
        );
        assert_eq!(SortField::from_token("read"), Some(SortField::ReadRate));
        assert_eq!(SortField::from_token("write"), Some(SortField::WriteRate));
        assert_eq!(SortField::from_token("pid"), Some(SortField::Pid));
        assert_eq!(SortField::from_token("name"), Some(SortField::Name));
        assert_eq!(SortField::from_token("age"), Some(SortField::Age));
    }

    #[test]
    fn sort_tokens_are_case_insensitive_but_never_prefix_matched() {
        assert_eq!(SortField::from_token(" CPU "), Some(SortField::Cpu));
        assert_eq!(SortField::from_token("MeM"), Some(SortField::MemoryShare));
        assert_eq!(SortField::from_token("c"), None);
        assert_eq!(SortField::from_token("cp"), None);
        assert_eq!(SortField::from_token(""), None);
    }

    #[test]
    fn consumption_columns_start_descending_and_text_columns_ascending() {
        assert!(SortField::Cpu.defaults_descending());
        assert!(SortField::Rss.defaults_descending());
        assert!(SortField::Age.defaults_descending());
        assert!(!SortField::Name.defaults_descending());
        assert!(!SortField::Pid.defaults_descending());
    }

    #[test]
    fn pushing_none_produces_no_effect() {
        let mut effects = Effects::new();

        effects.push(Effect::None);

        assert!(effects.is_empty());
        assert_eq!(effects.len(), 0);
    }

    #[test]
    fn redraw_requests_are_deduplicated_but_other_effects_are_not() {
        let mut effects = Effects::new();

        effects.push(Effect::RequestRedraw);
        effects.push(Effect::RequestRedraw);
        effects.push(Effect::RequestSample);
        effects.push(Effect::RequestSample);

        assert_eq!(
            effects.as_slice(),
            &[
                Effect::RequestRedraw,
                Effect::RequestSample,
                Effect::RequestSample
            ]
        );
    }

    #[test]
    fn effects_preserve_order_and_report_process_effects() {
        let mut effects = Effects::one(Effect::RequestRedraw);
        effects.push(Effect::SignalProcess {
            identity: identity(),
            signal: SignalKind::Term,
        });

        assert!(effects.touches_a_process());
        assert!(effects.contains(&Effect::RequestRedraw));
        assert_eq!(effects.iter().count(), 2);
        assert_eq!(
            effects.into_iter().last(),
            Some(Effect::SignalProcess {
                identity: identity(),
                signal: SignalKind::Term,
            })
        );
    }

    #[test]
    fn non_process_effects_need_no_revalidation() {
        for effect in [
            Effect::RequestRedraw,
            Effect::RequestSample,
            Effect::FetchProcessDetail(identity()),
            Effect::SetSensorInterest(true),
            Effect::RingBell,
            Effect::ReloadConfig,
            Effect::ExportSnapshot(PathBuf::from("/tmp/snapshot.json")),
            Effect::Shutdown,
        ] {
            assert!(
                !effect.requires_identity_revalidation(),
                "{effect:?} does not act on a process"
            );
        }
    }

    #[test]
    fn text_editing_actions_are_recognisable() {
        assert!(Action::InsertChar('q').is_text_editing());
        assert!(Action::DeleteBackward.is_text_editing());
        assert!(Action::SubmitInput.is_text_editing());
        assert!(!Action::Quit.is_text_editing());
        assert!(!Action::SelectNext.is_text_editing());
    }
}
