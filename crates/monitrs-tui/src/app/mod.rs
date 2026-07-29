//! Application state, and the reducer that is the only thing allowed to change it
//! (§10.2, §10.5).
//!
//! ```text
//! terminal events ----+
//! collector snapshots +--> bounded event channel --> reducer --> app state --> render
//! detail results -----+                              |
//!                                                    +--> explicit effects
//! ```
//!
//! [`apply`] takes one [`crate::event::Event`] and returns the
//! [`crate::action::Effects`] the outside world must perform. [`reduce`] does the
//! same for one [`crate::action::Action`]. Neither performs an effect:
//! no file is read, no signal is sent, no collector is called, and nothing is
//! drawn. That is what makes the §15.1 safety dialogs and the §6.2 keyboard model
//! testable without a machine in a particular state, and it is why every test in
//! this module can assert both *the next state* and *the emitted effects* (§17.4).
//!
//! # What the state owns, and what it derives
//!
//! Owned: the view, the focused panel, the overlay stack, the filter, the
//! ordering, the selection, the pins, the Time Lens position, the latest and the
//! *displayed* snapshot, the history ring, collector health, notices, terminal
//! size and render timing.
//!
//! Derived, and therefore never stored twice: the input mode (from the overlay
//! stack and the focused panel, §6.1), the visible rows (from the displayed
//! snapshot, the filter, the ordering and the tree toggle), the layout (from the
//! terminal size), the glyph set and colour depth (from the requested mode and the
//! captured environment), and the header timeline state.
//!
//! # Two snapshots, on purpose
//!
//! §2.1 pauses the *visible* timeline without stopping collection, so the state
//! keeps both the newest snapshot received ([`AppState::live_snapshot`]) and the
//! one on screen ([`AppState::snapshot`]). While live they are the same `Arc`.
//! While paused or scrubbed the displayed one is frozen, the ring keeps filling,
//! and process actions are refused (§15.1) — which is the whole reason it is safe
//! to keep showing a frozen process table.
//!
//! # Coalescing is counted, not hidden
//!
//! §10.3 requires snapshots to be coalesced when the UI is behind and forbids
//! queueing old ones. A snapshot that arrives before the previous one has been
//! rendered supersedes it and is counted in
//! [`CollectorHealth::coalesced_samples`]; a snapshot whose sequence is not newer
//! than the one already held is dropped outright. Neither is silent: §16.2 requires
//! collector lag to be displayed.
//!
//! # No `unwrap`, no panicking index
//!
//! Every lookup in this module is fallible and handled. A panic here corrupts the
//! terminal (§14.3), and the one place a user types arbitrary text — the filter —
//! is exactly where a bad index would be found.

mod command;
mod notice;
mod overlay;
mod reducer;
mod rows;
mod selection;
mod text;
mod timeline;
mod timing;

#[cfg(test)]
mod fixtures;

use core::time::Duration;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use monitrs_core::history::{HistoryConfig, HistoryRing};
use monitrs_core::model::{
    CollectorHealth, ProcessDetail, ProcessIdentity, ProcessSnapshot, SystemSnapshot,
};
use monitrs_core::process::{ProcessFilter, ProcessSort};
use monitrs_core::units::ByteUnits;
use ratatui::layout::Rect;

use crate::action::{PendingProcessAction, ViewId};
use crate::glyphs::{GlyphMode, GlyphSet, TerminalEnv};
use crate::keymap::{HelpSection, InputMode, KeyResolver, Keymap};
use crate::layout::Layout;
use crate::theme::{ColorDepth, ColorMode, Theme, ThemeId};

pub use command::{
    Command, CommandError, CommandHint, HINTS as COMMAND_HINTS, hints_for, parse as parse_command,
};
pub use notice::{MAX_NOTICES, Notice, NoticeKind, NoticeLog};
pub use overlay::{MAX_NICE, MIN_NICE, Overlay, OverlayKind, OverlayStack, ProcessActionStage};
pub use reducer::{apply, detail_line_count, help_line_count, reduce};
pub use rows::{ProcessRow, ProcessRows, TreeShape};
pub use selection::{Resync, Selection};
pub use text::{MAX_INPUT_CHARS, TextInput};
pub use timeline::{Timeline, TimelineStatus};
pub use timing::{FRAME_BUDGET, RenderTiming, TIMING_WINDOW};

/// How many processes may be pinned at once (§2.5).
///
/// The pins strip is a few lines of a dashboard, not a second process table, and
/// §10.3 forbids unbounded growth. Refusing the seventeenth pin with an
/// explanation is better than a strip that scrolls off the screen.
pub const MAX_PINNED_PROCESSES: usize = 16;

/// How long an idle interface waits before redrawing anyway.
///
/// The header carries a wall clock and relative ages (§5.5), so a completely idle
/// monitrs still has to repaint about once a second. Any shorter would be the
/// redraw busy loop §16.1 forbids; any longer and the clock would visibly stall.
pub const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(1_000);

/// Rows the process panel spends on its own header, excluded from a page jump.
const TABLE_HEADER_ROWS: u16 = 1;

/// Which panel has focus (§6.2 `Tab`).
///
/// Only the panels that contain something navigable are focusable: the header, the
/// status line and the resize notice are not places a cursor can be.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum PanelFocus {
    /// The Pressure Radar (§2.3).
    Pressure,
    /// The history sparklines — the Time Lens (§2.1).
    ///
    /// Focusing this panel is what puts the app in [`InputMode::TimeLens`], where
    /// §6.2 gives the arrow keys to seeking instead of to the selection.
    History,
    /// The focus-selected summary panel (§5.7 `Standard`, `Compact`).
    Summary,
    /// The process table, which is the primary view at every breakpoint.
    #[default]
    Processes,
    /// The pinned-process strip (§2.5).
    Pins,
    /// The per-interface network footer (§7.4).
    Network,
}

impl PanelFocus {
    /// Every focusable panel, in `Tab` order: top to bottom, left to right, as
    /// §5.5 lays them out.
    pub const ALL: [Self; 6] = [
        Self::Pressure,
        Self::History,
        Self::Summary,
        Self::Processes,
        Self::Pins,
        Self::Network,
    ];

    /// The panel name, used in the status line and in help.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pressure => "pressure",
            Self::History => "history",
            Self::Summary => "summary",
            Self::Processes => "processes",
            Self::Pins => "pins",
            Self::Network => "network",
        }
    }

    /// Whether this panel exists in `layout`.
    ///
    /// A panel the current breakpoint does not draw must not be focusable, or
    /// `Tab` would move the cursor somewhere invisible (§5.7).
    #[must_use]
    pub const fn is_present(self, layout: &Layout) -> bool {
        match self {
            Self::Pressure => layout.pressure.is_some(),
            Self::History => layout.history.is_some(),
            Self::Summary => layout.summary.is_some(),
            Self::Processes => layout.processes.is_some(),
            Self::Pins => layout.pins.is_some(),
            Self::Network => layout.network.is_some(),
        }
    }

    /// Whether focusing this panel hands the arrow keys to the Time Lens (§2.1).
    #[must_use]
    pub const fn is_time_lens(self) -> bool {
        matches!(self, Self::History)
    }
}

/// The presentation settings the reducer can change (§6.2 `t`, `g`, §6.3).
///
/// Requested values, not resolved ones: `Auto` has to survive in the state so that
/// [`AppState::glyph_set`] and [`AppState::color_depth`] can resolve it against the
/// captured environment every time, purely and without reading `getenv` from a
/// render path (§10.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplaySettings {
    /// The active theme (§5.3).
    pub theme: ThemeId,
    /// The requested glyph mode (§5.1).
    pub glyph_mode: GlyphMode,
    /// The requested colour mode (§5.2).
    pub color_mode: ColorMode,
    /// Whether the colour mode came from an explicit request rather than from a
    /// default, which decides whether it may override `NO_COLOR` (§5.2).
    pub color_explicit: bool,
    /// Whether byte sizes are shown in IEC or SI units.
    pub byte_units: ByteUnits,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            theme: ThemeId::default(),
            glyph_mode: GlyphMode::default(),
            color_mode: ColorMode::default(),
            color_explicit: false,
            byte_units: ByteUnits::Iec,
        }
    }
}

/// Everything the runtime decides before the first frame.
///
/// Separate from [`AppState`] so that configuration (§12) and the command line
/// have exactly one way in, and so a test can build a state in a specific
/// condition without reaching into private fields.
#[derive(Debug)]
pub struct AppSettings {
    /// The monotonic origin: history offsets and the first tick are measured from
    /// here (§8.1).
    pub started_at: Instant,
    /// Initial terminal size, as `(columns, rows)`.
    pub size: (u16, u16),
    /// The view to open on.
    pub view: ViewId,
    /// The initial process ordering (§12 `processes.sort`).
    pub sort: ProcessSort,
    /// Whether to start in tree mode (§7.2).
    pub tree_mode: bool,
    /// An initial filter, as typed.
    pub filter: String,
    /// Show only this user's processes (§7.2 user-only toggle).
    pub only_user: Option<u32>,
    /// Hide kernel threads (§7.2, Linux).
    pub hide_kernel_threads: bool,
    /// Presentation settings.
    pub display: DisplaySettings,
    /// The captured terminal environment, so glyph and colour resolution stays a
    /// pure function of state (§5.1, §5.2).
    pub env: TerminalEnv,
    /// The requested history configuration; out-of-range values are clamped and
    /// reported (§8.5).
    pub history: HistoryConfig,
    /// The requested sample interval (§12 `sampling.interval`).
    pub sample_interval: Duration,
    /// Where configuration was read from, for `config path` (§6.3).
    pub config_path: Option<PathBuf>,
    /// The active keymap, already validated (§12).
    pub keymap: Keymap,
    /// How long a multi-key sequence waits for its second key (§6.2).
    pub sequence_timeout: Duration,
}

impl Default for AppSettings {
    /// Defaults that read nothing: no environment, no clock beyond `now`, no file.
    ///
    /// `TerminalEnv::empty()` rather than `from_process()` on purpose — a state
    /// built by a test must not change behaviour because of the developer's locale.
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            size: (0, 0),
            view: ViewId::Overview,
            sort: ProcessSort::default(),
            tree_mode: false,
            filter: String::new(),
            only_user: None,
            hide_kernel_threads: false,
            display: DisplaySettings::default(),
            env: TerminalEnv::empty(),
            history: HistoryConfig::default(),
            sample_interval: monitrs_core::history::DEFAULT_SAMPLE_INTERVAL,
            config_path: None,
            keymap: Keymap::builtin(),
            sequence_timeout: crate::keymap::DEFAULT_SEQUENCE_TIMEOUT,
        }
    }
}

/// The whole application state (§10.2).
///
/// Mutated only by [`reduce`] and [`apply`], plus the two things the runtime knows
/// and the reducer cannot: [`AppState::record_render`] and
/// [`AppState::push_notice`].
#[derive(Debug)]
pub struct AppState {
    // ---- what is on screen ----
    pub(in crate::app) view: ViewId,
    pub(in crate::app) focus: PanelFocus,
    pub(in crate::app) overlays: OverlayStack,
    pub(in crate::app) columns: u16,
    pub(in crate::app) terminal_rows: u16,

    // ---- data ----
    pub(in crate::app) latest: Option<Arc<SystemSnapshot>>,
    pub(in crate::app) displayed: Option<Arc<SystemSnapshot>>,
    pub(in crate::app) history: HistoryRing,
    /// The *requested* history configuration, kept so that changing the interval or
    /// the span (§6.3) can rebuild the ring without inventing the other value.
    pub(in crate::app) history_config: HistoryConfig,
    pub(in crate::app) timeline: Timeline,
    pub(in crate::app) health: CollectorHealth,
    pub(in crate::app) coalesced_by_ui: u64,
    pub(in crate::app) detail: Option<Box<ProcessDetail>>,
    pub(in crate::app) detail_request: Option<ProcessIdentity>,

    // ---- the process table ----
    pub(in crate::app) filter_text: String,
    pub(in crate::app) filter: ProcessFilter,
    pub(in crate::app) only_user: Option<u32>,
    pub(in crate::app) hide_kernel_threads: bool,
    pub(in crate::app) sort: ProcessSort,
    pub(in crate::app) tree_mode: bool,
    pub(in crate::app) rows: ProcessRows,
    pub(in crate::app) selection: Selection,
    pub(in crate::app) pins: Vec<ProcessIdentity>,

    // ---- presentation ----
    pub(in crate::app) display: DisplaySettings,
    pub(in crate::app) env: TerminalEnv,

    // ---- runtime bookkeeping ----
    pub(in crate::app) resolver: KeyResolver,
    pub(in crate::app) clock: Instant,
    pub(in crate::app) sample_interval: Duration,
    pub(in crate::app) config_path: Option<PathBuf>,
    pub(in crate::app) notices: NoticeLog,
    pub(in crate::app) timing: RenderTiming,
    /// Whether the newest snapshot has reached the screen.
    ///
    /// The definition of "coalesced" in §10.3: a snapshot that supersedes one this
    /// flag says was never drawn is a coalesced sample.
    pub(in crate::app) unrendered_snapshot: bool,
    pub(in crate::app) should_quit: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(AppSettings::default())
    }
}

impl AppState {
    /// Builds the initial state.
    ///
    /// Reads nothing: the history ring is sized from `settings.history` (clamping
    /// and reporting per §8.5), and every clamp becomes a notice so the user learns
    /// that their configuration was adjusted.
    #[must_use]
    pub fn new(settings: AppSettings) -> Self {
        let AppSettings {
            started_at,
            size,
            view,
            sort,
            tree_mode,
            filter,
            only_user,
            hide_kernel_threads,
            display,
            env,
            history,
            sample_interval,
            config_path,
            keymap,
            sequence_timeout,
        } = settings;

        let ring = HistoryRing::with_config(history, started_at);
        let compiled = compile_filter(&filter, only_user, hide_kernel_threads);
        let mut state = Self {
            view,
            focus: PanelFocus::default(),
            overlays: OverlayStack::new(),
            columns: size.0,
            terminal_rows: size.1,
            latest: None,
            displayed: None,
            history: ring,
            history_config: history,
            timeline: Timeline::live(),
            health: CollectorHealth::default(),
            coalesced_by_ui: 0,
            detail: None,
            detail_request: None,
            filter_text: filter,
            filter: compiled,
            only_user,
            hide_kernel_threads,
            sort,
            tree_mode,
            rows: ProcessRows::empty(),
            selection: Selection::new(),
            pins: Vec::new(),
            display,
            env,
            resolver: KeyResolver::with_timeout(keymap, sequence_timeout),
            clock: started_at,
            sample_interval,
            config_path,
            notices: NoticeLog::new(),
            timing: RenderTiming::new(),
            unrendered_snapshot: false,
            should_quit: false,
        };

        // §8.5: the user must be warned that a configured value was adjusted.
        let clamps: Vec<String> = state
            .history
            .clamps()
            .iter()
            .map(|clamp| clamp.message())
            .collect();
        for message in clamps {
            state
                .notices
                .push(Notice::watch(NoticeKind::Config, message), started_at);
        }
        state
    }

    // ------------------------------------------------------------------ screen

    /// The active view (§6.2 `1`–`5`).
    #[must_use]
    pub const fn view(&self) -> ViewId {
        self.view
    }

    /// The focused panel (§6.2 `Tab`).
    #[must_use]
    pub const fn focus(&self) -> PanelFocus {
        self.focus
    }

    /// The open overlays, outermost first.
    #[must_use]
    pub fn overlays(&self) -> &[Overlay] {
        self.overlays.as_slice()
    }

    /// The overlay that owns the keyboard, if any.
    #[must_use]
    pub fn top_overlay(&self) -> Option<&Overlay> {
        self.overlays.top()
    }

    /// The current input mode (§6.1).
    ///
    /// Derived, never stored: it is whatever the topmost overlay implies, or
    /// [`InputMode::TimeLens`] when the history panel has focus, or
    /// [`InputMode::Normal`]. That is why an impossible combination — confirming
    /// with nothing pending, editing with nowhere to type — cannot be represented.
    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        if let Some(mode) = self.overlays.input_mode() {
            return mode;
        }
        if self.focus.is_time_lens() {
            return InputMode::TimeLens;
        }
        InputMode::Normal
    }

    /// The terminal size as `(columns, rows)`.
    #[must_use]
    pub const fn size(&self) -> (u16, u16) {
        (self.columns, self.terminal_rows)
    }

    /// The whole terminal as a rectangle, for layout resolution.
    #[must_use]
    pub const fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.columns,
            height: self.terminal_rows,
        }
    }

    /// The panel geometry for the current size (§5.7).
    #[must_use]
    pub fn layout(&self) -> Layout {
        Layout::resolve(self.area())
    }

    /// How many rows one `Ctrl-D` or `PageDown` moves.
    ///
    /// Derived from the process panel's real height, minus its header row, so a
    /// page is what the user can actually see. At least one row, so a page jump in
    /// a one-row panel still moves (§5.7 forbids a panic on a zero-area rect and
    /// this is the arithmetic that would divide by it).
    #[must_use]
    pub fn page_size(&self) -> usize {
        let height = self
            .layout()
            .processes
            .map_or(0, |area| area.height.saturating_sub(TABLE_HEADER_ROWS));
        usize::from(height.max(1))
    }

    // -------------------------------------------------------------------- data

    /// The snapshot on screen.
    ///
    /// The newest one while live; the frozen one while paused or in history (§2.1).
    #[must_use]
    pub fn snapshot(&self) -> Option<&Arc<SystemSnapshot>> {
        self.displayed.as_ref()
    }

    /// The newest snapshot received, whatever is on screen.
    ///
    /// This is the one the signal path must revalidate against: §15.1 requires the
    /// identity to be re-read immediately before an action, and a frozen snapshot
    /// is a record of the past.
    #[must_use]
    pub fn live_snapshot(&self) -> Option<&Arc<SystemSnapshot>> {
        self.latest.as_ref()
    }

    /// The bounded history ring (§8.5).
    #[must_use]
    pub const fn history(&self) -> &HistoryRing {
        &self.history
    }

    /// The Time Lens state (§2.1).
    #[must_use]
    pub const fn timeline(&self) -> Timeline {
        self.timeline
    }

    /// What the header must show: `LIVE`, `PAUSED`, or `HISTORY -MM:SS` (§2.1).
    #[must_use]
    pub fn timeline_status(&self) -> TimelineStatus {
        self.timeline.status(&self.history)
    }

    /// Whether process-control actions may be offered (§2.1, §15.1).
    #[must_use]
    pub const fn allows_process_actions(&self) -> bool {
        self.timeline.allows_process_actions()
    }

    /// Collector timing and our own overhead (§7.5, §26).
    #[must_use]
    pub const fn health(&self) -> &CollectorHealth {
        &self.health
    }

    /// The detail of the process the overlay is showing (§7.5).
    #[must_use]
    pub fn detail(&self) -> Option<&ProcessDetail> {
        self.detail.as_deref()
    }

    /// The identity whose detail has been requested but not yet answered.
    #[must_use]
    pub const fn detail_request(&self) -> Option<ProcessIdentity> {
        self.detail_request
    }

    // ----------------------------------------------------------- process table

    /// The filter as the user typed it.
    #[must_use]
    pub fn filter_text(&self) -> &str {
        &self.filter_text
    }

    /// The compiled filter, including the user-only and kernel-thread toggles.
    #[must_use]
    pub const fn filter(&self) -> &ProcessFilter {
        &self.filter
    }

    /// The active ordering (§7.2).
    #[must_use]
    pub const fn sort(&self) -> ProcessSort {
        self.sort
    }

    /// Whether the table is in tree mode (§6.2 `f`).
    #[must_use]
    pub const fn is_tree_view(&self) -> bool {
        self.tree_mode
    }

    /// The visible rows, in display order.
    #[must_use]
    pub const fn rows(&self) -> &ProcessRows {
        &self.rows
    }

    /// The selection, tracked by stable identity (§7.2).
    #[must_use]
    pub const fn selection(&self) -> Selection {
        self.selection
    }

    /// The selected process's identity, if anything is selected.
    #[must_use]
    pub const fn selected(&self) -> Option<ProcessIdentity> {
        self.selection.identity()
    }

    /// The selected row index.
    #[must_use]
    pub const fn selected_row(&self) -> Option<usize> {
        self.selection.row()
    }

    /// The selected process's row data, from the displayed snapshot.
    #[must_use]
    pub fn selected_process(&self) -> Option<&ProcessSnapshot> {
        let snapshot = self.displayed.as_ref()?;
        self.rows.process(snapshot, self.selection.row()?)
    }

    /// The pinned processes, in the order they were pinned (§2.5).
    #[must_use]
    pub fn pins(&self) -> &[ProcessIdentity] {
        &self.pins
    }

    /// Whether `identity` is pinned.
    ///
    /// By identity, so a reused PID inherits nothing (§2.5, §26).
    #[must_use]
    pub fn is_pinned(&self, identity: ProcessIdentity) -> bool {
        self.pins.contains(&identity)
    }

    /// The action awaiting confirmation, if the chain has reached that stage
    /// (§15.1).
    #[must_use]
    pub fn pending_process_action(&self) -> Option<PendingProcessAction> {
        match self.overlays.find(OverlayKind::ProcessAction) {
            Some(Overlay::ProcessAction(stage)) => stage.pending(),
            _ => None,
        }
    }

    /// The process action mid-chain, at whatever stage it is (§15.1).
    #[must_use]
    pub fn process_action_stage(&self) -> Option<ProcessActionStage> {
        match self.overlays.find(OverlayKind::ProcessAction) {
            Some(Overlay::ProcessAction(stage)) => Some(*stage),
            _ => None,
        }
    }

    // ------------------------------------------------------------ presentation

    /// The presentation settings (§5.1, §5.2, §5.3).
    #[must_use]
    pub const fn display(&self) -> DisplaySettings {
        self.display
    }

    /// The active theme.
    #[must_use]
    pub const fn theme(&self) -> &'static Theme {
        self.display.theme.theme()
    }

    /// The resolved glyph set (§5.1).
    #[must_use]
    pub fn glyph_set(&self) -> GlyphSet {
        GlyphSet::resolve(self.display.glyph_mode, &self.env)
    }

    /// The resolved colour depth (§5.2).
    #[must_use]
    pub fn color_depth(&self) -> ColorDepth {
        self.display
            .color_mode
            .resolve(&self.env, self.display.color_explicit)
    }

    /// The captured terminal environment.
    #[must_use]
    pub const fn env(&self) -> &TerminalEnv {
        &self.env
    }

    // ------------------------------------------------------------------ runtime

    /// The keymap in force, for rendering help and for the palette's key hints.
    #[must_use]
    pub const fn keymap(&self) -> &Keymap {
        self.resolver.keymap()
    }

    /// The generated help for the current mode (§7.6).
    #[must_use]
    pub fn help(&self) -> Vec<HelpSection> {
        self.keymap().help(self.input_mode())
    }

    /// The most recent monotonic time the reducer has been told about.
    ///
    /// Ticks and snapshots advance it; nothing in this crate calls
    /// `Instant::now()` outside of `AppSettings::default`, so a test drives time
    /// explicitly and a reducer decision is reproducible (§8.1).
    #[must_use]
    pub const fn clock(&self) -> Instant {
        self.clock
    }

    /// The requested sample interval (§12 `sampling.interval`).
    ///
    /// The runtime re-arms its sampler from this after every reduction: §10.5's
    /// effect list has no variant for "change the interval", and inventing one
    /// would put a scheduling decision in the effect queue behind a signal.
    #[must_use]
    pub const fn sample_interval(&self) -> Duration {
        self.sample_interval
    }

    /// Where configuration was read from, if anywhere (§6.3 `config path`).
    #[must_use]
    pub fn config_path(&self) -> Option<&std::path::Path> {
        self.config_path.as_deref()
    }

    /// The notices to show (§14.1).
    #[must_use]
    pub fn notices(&self) -> &[Notice] {
        self.notices.as_slice()
    }

    /// The notice log, for the panel that renders counts and severities.
    #[must_use]
    pub const fn notice_log(&self) -> &NoticeLog {
        &self.notices
    }

    /// Records a notice the reducer could not produce itself.
    ///
    /// The one thing the runtime knows and the reducer does not: the outcome of an
    /// executed [`crate::action::Effect`] — whether the signal was delivered,
    /// whether the export was written, whether the reloaded configuration parsed.
    /// §10.2's `ConfigReloaded` payload is opaque to this crate by design (§10.1),
    /// so the runtime inspects it and reports here.
    pub fn push_notice(&mut self, notice: Notice) {
        self.notices.push(notice, self.clock);
    }

    /// Frame timing (§16.1, §26).
    #[must_use]
    pub const fn render_timing(&self) -> &RenderTiming {
        &self.timing
    }

    /// Records that a frame was drawn.
    ///
    /// Must be called after every render: it is what clears the "this state has
    /// not been shown yet" flag, and therefore what makes the coalescing count
    /// mean what §10.3 says it means.
    pub fn record_render(&mut self, at: Instant, duration: Duration) {
        self.advance_clock(at);
        self.timing.record(at, duration);
        self.unrendered_snapshot = false;
    }

    /// Whether the newest snapshot has not been drawn yet (§10.3).
    #[must_use]
    pub const fn has_unrendered_snapshot(&self) -> bool {
        self.unrendered_snapshot
    }

    /// Whether the app has been asked to exit (§6.2 `q`, `Ctrl-C`).
    #[must_use]
    pub const fn should_quit(&self) -> bool {
        self.should_quit
    }

    // ------------------------------------------------------- internal mutation

    /// Advances the monotonic clock, never backwards.
    pub(in crate::app) fn advance_clock(&mut self, now: Instant) {
        if now > self.clock {
            self.clock = now;
        }
    }

    /// Records a notice at the current clock.
    pub(in crate::app) fn notify(&mut self, notice: Notice) {
        self.notices.push(notice, self.clock);
    }

    /// Rebuilds the visible rows and re-synchronises the selection (§7.2).
    ///
    /// The single place rows are built, so the §7.2 stability rules cannot be
    /// bypassed by a code path that forgot to call them.
    pub(in crate::app) fn resync_rows(&mut self) -> Resync {
        self.rows = ProcessRows::build(
            self.displayed.as_deref(),
            &self.filter,
            self.sort,
            self.tree_mode,
        );
        self.selection.resync(&self.rows)
    }

    /// Replaces the filter text and recompiles the filter.
    pub(in crate::app) fn set_filter_text(&mut self, text: String) -> bool {
        if self.filter_text == text {
            return false;
        }
        self.filter_text = text;
        self.filter = compile_filter(&self.filter_text, self.only_user, self.hide_kernel_threads);
        true
    }

    /// Counts a snapshot that superseded an unrendered one (§10.3).
    ///
    /// Kept in two places on purpose: `coalesced_by_ui` is this process's own
    /// running total, and the copy in [`CollectorHealth`] is what the Inspect
    /// screen renders. A fresh health record from the collector carries only the
    /// samples *it* dropped, so [`AppState::absorb_health`] adds ours back rather
    /// than letting the display forget them.
    pub(in crate::app) fn count_coalesced(&mut self) {
        self.coalesced_by_ui = self.coalesced_by_ui.saturating_add(1);
        self.health.coalesced_samples = self.health.coalesced_samples.saturating_add(1);
    }

    /// Takes a fresh health record, preserving our own coalescing count.
    pub(in crate::app) fn absorb_health(&mut self, mut health: CollectorHealth) {
        health.coalesced_samples = health
            .coalesced_samples
            .saturating_add(self.coalesced_by_ui);
        self.health = health;
    }

    /// Moves focus to the next or previous panel present in the layout (§6.2).
    pub(in crate::app) fn cycle_focus(&mut self, forward: bool) -> bool {
        let layout = self.layout();
        let present: Vec<PanelFocus> = PanelFocus::ALL
            .into_iter()
            .filter(|panel| panel.is_present(&layout))
            .collect();
        if present.is_empty() {
            return false;
        }
        let target = match present.iter().position(|panel| *panel == self.focus) {
            Some(index) if forward => (index + 1) % present.len(),
            Some(index) => (index + present.len() - 1) % present.len(),
            // The focused panel is not drawn at this size: start from the top of
            // the order rather than nowhere.
            None => 0,
        };
        let Some(next) = present.get(target).copied() else {
            return false;
        };
        if next == self.focus {
            return false;
        }
        self.focus = next;
        true
    }

    /// Moves focus back to a panel that exists, after a resize (§5.7).
    pub(in crate::app) fn revalidate_focus(&mut self) -> bool {
        let layout = self.layout();
        if self.focus.is_present(&layout) {
            return false;
        }
        let fallback = PanelFocus::ALL
            .into_iter()
            .find(|panel| panel.is_present(&layout));
        match fallback {
            Some(panel) if panel != self.focus => {
                self.focus = panel;
                true
            }
            // Nothing is drawn at all (the §5.7 resize notice): keep the focus so
            // it is restored when the terminal grows again.
            _ => false,
        }
    }
}

/// Compiles the typed filter together with the two §7.2 toggles.
fn compile_filter(text: &str, only_user: Option<u32>, hide_kernel_threads: bool) -> ProcessFilter {
    ProcessFilter::parse(text)
        .with_only_user(only_user)
        .with_hidden_kernel_threads(hide_kernel_threads)
}

/// The next glyph mode in the `g` cycle (§6.2).
///
/// `Auto` first, so one more press always returns to letting the terminal decide.
/// [`GlyphMode`] deliberately exposes no `next`: cycling is an interaction concern,
/// not a property of the mode.
pub(in crate::app) const fn next_glyph_mode(mode: GlyphMode) -> GlyphMode {
    match mode {
        GlyphMode::Auto => GlyphMode::Unicode,
        GlyphMode::Unicode => GlyphMode::Ascii,
        GlyphMode::Ascii => GlyphMode::Auto,
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::history::DEFAULT_HISTORY_DURATION;
    use monitrs_core::process::{ProcessSortKey, SortDirection};

    use super::fixtures::{Fake, arc_snapshot, epoch};
    use super::*;

    fn state() -> AppState {
        AppState::new(AppSettings {
            started_at: epoch(),
            size: (160, 48),
            ..AppSettings::default()
        })
    }

    #[test]
    fn a_fresh_state_shows_nothing_and_is_live() {
        let state = state();

        assert_eq!(state.view(), ViewId::Overview);
        assert_eq!(state.focus(), PanelFocus::Processes);
        assert_eq!(state.input_mode(), InputMode::Normal);
        assert!(state.snapshot().is_none());
        assert!(state.live_snapshot().is_none());
        assert!(state.rows().is_empty());
        assert_eq!(state.selected(), None);
        assert!(state.pins().is_empty());
        assert_eq!(state.timeline_status(), TimelineStatus::Live);
        assert!(state.allows_process_actions());
        assert!(state.pending_process_action().is_none());
        assert!(state.notices().is_empty());
        assert!(!state.should_quit());
        assert!(!state.has_unrendered_snapshot());
        assert_eq!(state.render_timing().frames(), 0);
    }

    #[test]
    fn the_input_mode_follows_the_focused_panel_into_the_time_lens() {
        let mut state = state();
        assert_eq!(state.input_mode(), InputMode::Normal);

        state.focus = PanelFocus::History;
        assert_eq!(
            state.input_mode(),
            InputMode::TimeLens,
            "§6.1: focusing the lens gives the arrow keys to seeking"
        );

        // An overlay outranks the panel: it owns the keyboard.
        state.overlays.push(Overlay::Help { scroll: 0 });
        assert_eq!(state.input_mode(), InputMode::Help);
    }

    #[test]
    fn tab_only_visits_panels_the_breakpoint_actually_draws() {
        // Compact draws header, summary, processes and status only (§5.7).
        let mut state = AppState::new(AppSettings {
            started_at: epoch(),
            size: (90, 24),
            ..AppSettings::default()
        });
        let layout = state.layout();
        assert!(layout.pressure.is_none());
        assert!(layout.pins.is_none());

        let mut visited = Vec::new();
        for _ in 0..4 {
            let _ = state.cycle_focus(true);
            visited.push(state.focus());
        }
        assert!(
            visited
                .iter()
                .all(|panel| panel.is_present(&state.layout())),
            "visited {visited:?} at a size that draws only some panels"
        );
    }

    #[test]
    fn focus_falls_back_to_a_drawn_panel_when_the_terminal_shrinks() {
        let mut state = state();
        state.focus = PanelFocus::Pins;
        assert!(state.focus.is_present(&state.layout()));

        state.columns = 90;
        state.terminal_rows = 24;
        assert!(state.revalidate_focus());
        assert!(state.focus().is_present(&state.layout()));
        assert!(!state.revalidate_focus(), "already valid");
    }

    #[test]
    fn a_page_is_the_visible_height_and_never_zero() {
        let wide = state();
        assert!(wide.page_size() > 1);

        let unusable = AppState::new(AppSettings {
            started_at: epoch(),
            size: (0, 0),
            ..AppSettings::default()
        });
        assert_eq!(
            unusable.page_size(),
            1,
            "a zero-area layout must not produce a zero page (§5.7)"
        );
    }

    #[test]
    fn history_configuration_clamps_are_reported_as_notices() {
        let state = AppState::new(AppSettings {
            started_at: epoch(),
            history: HistoryConfig {
                duration: Duration::from_secs(60 * 60 * 24),
                ..HistoryConfig::default()
            },
            ..AppSettings::default()
        });

        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.kind == NoticeKind::Config),
            "§8.5 requires the user to be warned about a clamp"
        );
        assert!(state.history().limits().effective_duration() <= DEFAULT_HISTORY_DURATION * 12);
    }

    #[test]
    fn the_filter_text_and_the_compiled_filter_stay_in_step() {
        let mut state = AppState::new(AppSettings {
            started_at: epoch(),
            size: (160, 48),
            filter: "rustc".to_owned(),
            hide_kernel_threads: true,
            ..AppSettings::default()
        });
        assert_eq!(state.filter_text(), "rustc");
        assert!(state.filter().is_active());
        assert!(state.filter().hides_kernel_threads());

        assert!(state.set_filter_text("postgres".to_owned()));
        assert_eq!(
            state.filter().pattern().map(ToString::to_string),
            Some("postgres".to_owned())
        );
        assert!(
            state.filter().hides_kernel_threads(),
            "the toggles survive a new pattern"
        );
        assert!(!state.set_filter_text("postgres".to_owned()));
    }

    #[test]
    fn rows_are_rebuilt_from_the_displayed_snapshot_only() {
        let mut state = state();
        state.latest = Some(arc_snapshot(1, &[Fake::new(1, 11, "launchd").cpu(1.0)]));
        assert_eq!(
            state.resync_rows(),
            Resync::Empty,
            "nothing is displayed yet, so there are no rows"
        );

        state.displayed = state.latest.clone();
        assert_eq!(state.resync_rows(), Resync::Initialised { row: 0 });
        assert_eq!(state.selected(), Some(ProcessIdentity::new(1, 11)));
        assert_eq!(
            state.selected_process().map(|process| &*process.name),
            Some("launchd")
        );
    }

    #[test]
    fn the_ui_keeps_its_own_coalescing_count_across_a_health_update() {
        let mut state = state();
        state.count_coalesced();
        state.count_coalesced();
        assert_eq!(state.health().coalesced_samples, 2);

        state.absorb_health(CollectorHealth {
            dropped_samples: 7,
            ..CollectorHealth::default()
        });

        assert_eq!(state.health().dropped_samples, 7);
        assert_eq!(
            state.health().coalesced_samples,
            2,
            "a fresh collector record must not erase what the UI coalesced"
        );
    }

    #[test]
    fn the_clock_never_moves_backwards() {
        let mut state = state();
        let base = state.clock();
        state.advance_clock(base + Duration::from_secs(5));
        assert_eq!(state.clock(), base + Duration::from_secs(5));
        state.advance_clock(base);
        assert_eq!(
            state.clock(),
            base + Duration::from_secs(5),
            "§8.1: monotonic ordering"
        );
    }

    #[test]
    fn recording_a_frame_clears_the_pending_flag_and_times_it() {
        let mut state = state();
        state.unrendered_snapshot = true;
        let at = state.clock() + Duration::from_millis(20);

        state.record_render(at, Duration::from_millis(6));

        assert!(!state.has_unrendered_snapshot());
        assert_eq!(state.render_timing().frames(), 1);
        assert_eq!(state.render_timing().last(), Duration::from_millis(6));
        assert_eq!(state.clock(), at, "rendering advances the clock");
    }

    #[test]
    fn the_glyph_and_colour_settings_resolve_purely_from_captured_state() {
        let ascii = AppState::new(AppSettings {
            started_at: epoch(),
            display: DisplaySettings {
                glyph_mode: GlyphMode::Ascii,
                ..DisplaySettings::default()
            },
            ..AppSettings::default()
        });
        assert!(ascii.glyph_set().is_ascii());

        let no_color = AppState::new(AppSettings {
            started_at: epoch(),
            env: TerminalEnv::empty().with_no_color("1"),
            ..AppSettings::default()
        });
        assert_eq!(no_color.color_depth(), ColorDepth::Off);
    }

    #[test]
    fn the_glyph_cycle_returns_to_auto() {
        let mut mode = GlyphMode::Auto;
        let mut seen = Vec::new();
        for _ in 0..3 {
            mode = next_glyph_mode(mode);
            seen.push(mode);
        }
        assert_eq!(
            seen,
            vec![GlyphMode::Unicode, GlyphMode::Ascii, GlyphMode::Auto]
        );
    }

    #[test]
    fn help_is_generated_for_the_mode_the_app_is_actually_in() {
        let mut state = state();
        let normal = state.help();
        assert!(!normal.is_empty());

        state.overlays.push(Overlay::FilterEdit {
            input: TextInput::new(),
        });
        let editing = state.help();
        assert_ne!(
            normal, editing,
            "§7.6: help is generated from the active keymap and mode"
        );
    }

    #[test]
    fn settings_carry_the_configured_ordering_through_to_the_state() {
        let state = AppState::new(AppSettings {
            started_at: epoch(),
            sort: ProcessSort::new(ProcessSortKey::Memory, SortDirection::Ascending),
            tree_mode: true,
            view: ViewId::Processes,
            ..AppSettings::default()
        });
        assert_eq!(state.sort().key, ProcessSortKey::Memory);
        assert_eq!(state.sort().direction, SortDirection::Ascending);
        assert!(state.is_tree_view());
        assert_eq!(state.view(), ViewId::Processes);
    }

    #[test]
    fn a_notice_pushed_by_the_runtime_lands_at_the_current_clock() {
        let mut state = state();
        state.advance_clock(state.clock() + Duration::from_secs(2));
        state.push_notice(Notice::info(NoticeKind::Export, "wrote 12 KiB"));

        let notice = state.notices().last().expect("one notice");
        assert_eq!(notice.last_seen, Some(state.clock()));
        assert_eq!(state.notice_log().len(), 1);
    }
}
