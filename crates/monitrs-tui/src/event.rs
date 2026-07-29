//! Everything that can arrive at the reducer (§10.2).
//!
//! One bounded channel carries terminal input, collector snapshots, on-demand
//! detail results and timer ticks into a single reducer, so there is exactly one
//! place where state changes and exactly one ordering of those changes.
//!
//! # Why the input model is not Crossterm's
//!
//! [`TerminalEvent`] is a normalized re-statement of Crossterm's event types
//! rather than a re-export. Two reasons, both load-bearing:
//!
//! * **Terminals disagree about modifiers.** Depending on the terminal and the
//!   active keyboard protocol, `G` arrives as `Char('G')` with or without
//!   `SHIFT`, and `Ctrl-D` as `Char('d')` or `Char('D')` with `CONTROL`. A keymap
//!   that matches on the raw event is correct on one terminal and broken on the
//!   next, so [`KeyPress::new`] normalizes once, at the edge (§6.2).
//! * **Key *release* must never act.** Under the Kitty keyboard protocol, and
//!   always on Windows, Crossterm reports releases too. Treating a release as a
//!   press double-fires every binding — including the process-control ones
//!   (§15.1).
//!
//! Bracketed paste is compiled into Crossterm (feature unification through
//! `ratatui-crossterm` turns it on) but [`crate::terminal::TerminalGuard`] never
//! enables the *mode*, so a terminal has no reason to send a paste sequence and
//! pasted text arrives as ordinary key events. A paste event that shows up
//! anyway — because something else left the terminal in bracketed-paste mode — is
//! dropped rather than typed: inserting an unbounded, unreviewed string into a
//! filter or a palette command is worse than ignoring it (§6.3 forbids executing
//! arbitrary input).

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton as CtButton, MouseEvent, MouseEventKind,
};
use monitrs_core::model::{CollectorHealth, ProcessDetailResult, SystemSnapshot};

/// Modifier keys, reduced to the three a terminal reports reliably.
///
/// `Super`/`Hyper`/`Meta` are intentionally absent: they are not portable, and a
/// modifier the keymap cannot model must not silently degrade into an
/// unmodified key. [`Modifiers::from_crossterm`] rejects them instead (§15.1).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Modifiers {
    /// Control was held.
    pub ctrl: bool,
    /// Alt (Option on macOS) was held.
    pub alt: bool,
    /// Shift was held.
    pub shift: bool,
}

impl Modifiers {
    /// No modifiers.
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };

    /// Control only.
    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
    };

    /// Alt only.
    pub const ALT: Self = Self {
        ctrl: false,
        alt: true,
        shift: false,
    };

    /// Shift only.
    pub const SHIFT: Self = Self {
        ctrl: false,
        alt: false,
        shift: true,
    };

    /// Whether no modifier at all is held.
    #[must_use]
    pub const fn is_none(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }

    /// Translates Crossterm modifiers, rejecting the ones this keymap does not
    /// model so that, say, `Cmd-K` can never resolve to plain `K`.
    #[must_use]
    pub fn from_crossterm(modifiers: KeyModifiers) -> Option<Self> {
        let unmodelled = KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META;
        if modifiers.intersects(unmodelled) {
            return None;
        }
        Some(Self {
            ctrl: modifiers.contains(KeyModifiers::CONTROL),
            alt: modifiers.contains(KeyModifiers::ALT),
            shift: modifiers.contains(KeyModifiers::SHIFT),
        })
    }

    /// The `Ctrl-`/`Alt-`/`Shift-` prefix used when rendering a binding in help.
    #[must_use]
    pub fn prefix(self) -> String {
        let mut prefix = String::new();
        if self.ctrl {
            prefix.push_str("Ctrl-");
        }
        if self.alt {
            prefix.push_str("Alt-");
        }
        if self.shift {
            prefix.push_str("Shift-");
        }
        prefix
    }
}

/// A key, independent of which terminal reported it.
///
/// Keys monitrs has no use for (media keys, lock keys, bare modifier keys) are
/// not represented: [`KeyPress::from_crossterm`] returns `None` for them, which
/// is how an unbindable key stays unbindable.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Key {
    /// A character-producing key. Always the character the terminal reported,
    /// so `G` and `g` are different keys.
    Char(char),
    /// Return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Shift-Tab, which terminals report as its own key.
    BackTab,
    /// Backspace.
    Backspace,
    /// Delete (forward delete).
    Delete,
    /// Insert.
    Insert,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// A function key, `F1` upward.
    Function(u8),
}

impl Key {
    /// The name used in generated help. ASCII only, because §5.1 requires the
    /// whole UI to work in strict-ASCII mode.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Char(' ') => "Space".to_owned(),
            Self::Char(c) => c.to_string(),
            Self::Enter => "Enter".to_owned(),
            Self::Escape => "Esc".to_owned(),
            Self::Tab => "Tab".to_owned(),
            // Normalization strips the redundant Shift modifier, so the name has
            // to carry it: §6.2 calls this key `Shift-Tab`.
            Self::BackTab => "Shift-Tab".to_owned(),
            Self::Backspace => "Backspace".to_owned(),
            Self::Delete => "Delete".to_owned(),
            Self::Insert => "Insert".to_owned(),
            Self::Left => "Left".to_owned(),
            Self::Right => "Right".to_owned(),
            Self::Up => "Up".to_owned(),
            Self::Down => "Down".to_owned(),
            Self::Home => "Home".to_owned(),
            Self::End => "End".to_owned(),
            Self::PageUp => "PageUp".to_owned(),
            Self::PageDown => "PageDown".to_owned(),
            Self::Function(n) => format!("F{n}"),
        }
    }

    /// Whether this key produces text that belongs in a text field.
    ///
    /// Control characters are excluded, so `Enter` and `Backspace` keep their
    /// editing meaning in `FilterEdit` instead of being inserted (§6.1).
    ///
    /// Not `const`: `char::is_control` only became usable in a `const` context in
    /// Rust 1.97, and the workspace MSRV is 1.95.
    #[must_use]
    pub fn typed_char(self) -> Option<char> {
        match self {
            Self::Char(c) if !c.is_control() => Some(c),
            _ => None,
        }
    }

    /// Translates a Crossterm key code, returning `None` for keys monitrs does
    /// not bind.
    #[must_use]
    pub const fn from_crossterm(code: KeyCode) -> Option<Self> {
        match code {
            KeyCode::Char(c) => Some(Self::Char(c)),
            KeyCode::Enter => Some(Self::Enter),
            KeyCode::Esc => Some(Self::Escape),
            KeyCode::Tab => Some(Self::Tab),
            KeyCode::BackTab => Some(Self::BackTab),
            KeyCode::Backspace => Some(Self::Backspace),
            KeyCode::Delete => Some(Self::Delete),
            KeyCode::Insert => Some(Self::Insert),
            KeyCode::Left => Some(Self::Left),
            KeyCode::Right => Some(Self::Right),
            KeyCode::Up => Some(Self::Up),
            KeyCode::Down => Some(Self::Down),
            KeyCode::Home => Some(Self::Home),
            KeyCode::End => Some(Self::End),
            KeyCode::PageUp => Some(Self::PageUp),
            KeyCode::PageDown => Some(Self::PageDown),
            KeyCode::F(n) => Some(Self::Function(n)),
            KeyCode::Null
            | KeyCode::CapsLock
            | KeyCode::ScrollLock
            | KeyCode::NumLock
            | KeyCode::PrintScreen
            | KeyCode::Pause
            | KeyCode::Menu
            | KeyCode::KeypadBegin
            | KeyCode::Media(_)
            | KeyCode::Modifier(_) => None,
        }
    }
}

/// A normalized key press: what the keymap matches on.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyPress {
    /// The key.
    pub key: Key,
    /// The modifiers still meaningful after normalization.
    pub modifiers: Modifiers,
}

impl KeyPress {
    /// Builds a normalized press. Both the keymap table and the input thread go
    /// through here, so a binding and a real key press cannot disagree.
    ///
    /// Normalization (§6.2):
    ///
    /// * `Shift` is dropped when the key already encodes it — a character key
    ///   (`G` is not `g`) or `BackTab` (which *is* `Shift-Tab`). Terminals are
    ///   inconsistent about reporting it, so keeping it would make bindings
    ///   terminal-dependent.
    /// * `Ctrl` with an ASCII letter is folded to lower case: no terminal
    ///   reliably distinguishes `Ctrl-D` from `Ctrl-Shift-D`.
    #[must_use]
    pub const fn new(key: Key, modifiers: Modifiers) -> Self {
        let mut modifiers = modifiers;
        match key {
            Key::Char(_) | Key::BackTab => modifiers.shift = false,
            _ => {}
        }
        let key = match key {
            Key::Char(c) if modifiers.ctrl => Key::Char(c.to_ascii_lowercase()),
            other => other,
        };
        Self { key, modifiers }
    }

    /// An unmodified press.
    #[must_use]
    pub const fn plain(key: Key) -> Self {
        Self::new(key, Modifiers::NONE)
    }

    /// A character key with no modifiers.
    #[must_use]
    pub const fn char(c: char) -> Self {
        Self::new(Key::Char(c), Modifiers::NONE)
    }

    /// A `Ctrl`-modified character key.
    #[must_use]
    pub const fn ctrl(c: char) -> Self {
        Self::new(Key::Char(c), Modifiers::CTRL)
    }

    /// The label used in generated help, e.g. `Ctrl-C` or `Space`.
    #[must_use]
    pub fn label(&self) -> String {
        let key = match self.key {
            // §6.2 writes control chords in upper case (`Ctrl-C`) even though
            // matching folds them to lower case.
            Key::Char(c) if self.modifiers.ctrl => c.to_ascii_uppercase().to_string(),
            other => other.label(),
        };
        format!("{}{key}", self.modifiers.prefix())
    }

    /// The character this press should insert into a text field, if any.
    ///
    /// `None` when a modifier other than `Shift` is held: `Ctrl-U` clears the
    /// input, it does not type a `u`.
    #[must_use]
    pub fn typed_char(&self) -> Option<char> {
        if self.modifiers.ctrl || self.modifiers.alt {
            return None;
        }
        self.key.typed_char()
    }

    /// Translates a Crossterm key event.
    ///
    /// Returns `None` for key releases, for keys monitrs does not model, and for
    /// unmodelled modifier combinations.
    #[must_use]
    pub fn from_crossterm(event: KeyEvent) -> Option<Self> {
        // Repeat is accepted so that holding `j` keeps scrolling; Release never
        // is, or every binding would fire twice (§15.1).
        if matches!(event.kind, KeyEventKind::Release) {
            return None;
        }
        let key = Key::from_crossterm(event.code)?;
        let modifiers = Modifiers::from_crossterm(event.modifiers)?;
        Some(Self::new(key, modifiers))
    }
}

/// A mouse button.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Middle button.
    Middle,
    /// Right button.
    Right,
}

impl MouseButton {
    /// Translates a Crossterm button.
    #[must_use]
    pub const fn from_crossterm(button: CtButton) -> Self {
        match button {
            CtButton::Left => Self::Left,
            CtButton::Middle => Self::Middle,
            CtButton::Right => Self::Right,
        }
    }
}

/// What a mouse did.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MouseAction {
    /// A button went down.
    Down(MouseButton),
    /// A button came up.
    Up(MouseButton),
    /// The pointer moved with a button held.
    Drag(MouseButton),
    /// The pointer moved with no button held.
    Moved,
    /// Wheel/trackpad scroll up.
    ScrollUp,
    /// Wheel/trackpad scroll down.
    ScrollDown,
    /// Horizontal scroll left.
    ScrollLeft,
    /// Horizontal scroll right.
    ScrollRight,
}

/// A mouse event at a cell position.
///
/// Only produced when the guard enabled mouse capture, which is off by default
/// (§12 `display.mouse = false`). No binding in the default keymap consumes
/// mouse input: §6.2 defines keys only, and inventing mouse semantics would be
/// unspecified behaviour.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MouseInput {
    /// What happened.
    pub action: MouseAction,
    /// Zero-based column.
    pub column: u16,
    /// Zero-based row.
    pub row: u16,
    /// Modifiers held at the time.
    pub modifiers: Modifiers,
}

impl MouseInput {
    /// Translates a Crossterm mouse event, rejecting unmodelled modifiers for
    /// the same reason [`Modifiers::from_crossterm`] does.
    #[must_use]
    pub fn from_crossterm(event: MouseEvent) -> Option<Self> {
        let action = match event.kind {
            MouseEventKind::Down(button) => MouseAction::Down(MouseButton::from_crossterm(button)),
            MouseEventKind::Up(button) => MouseAction::Up(MouseButton::from_crossterm(button)),
            MouseEventKind::Drag(button) => MouseAction::Drag(MouseButton::from_crossterm(button)),
            MouseEventKind::Moved => MouseAction::Moved,
            MouseEventKind::ScrollUp => MouseAction::ScrollUp,
            MouseEventKind::ScrollDown => MouseAction::ScrollDown,
            MouseEventKind::ScrollLeft => MouseAction::ScrollLeft,
            MouseEventKind::ScrollRight => MouseAction::ScrollRight,
        };
        Some(Self {
            action,
            column: event.column,
            row: event.row,
            modifiers: Modifiers::from_crossterm(event.modifiers)?,
        })
    }
}

/// Terminal input, normalized (§10.2).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TerminalEvent {
    /// A key was pressed.
    Key(KeyPress),
    /// The mouse moved or was clicked. Only when mouse capture is enabled.
    Mouse(MouseInput),
    /// The terminal was resized. §17.5 requires this path to be tested.
    Resize {
        /// New width in cells.
        columns: u16,
        /// New height in cells.
        rows: u16,
    },
    /// The terminal window gained focus. Only reported when focus-change
    /// reporting is enabled, which this crate's guard does not do.
    FocusGained,
    /// The terminal window lost focus. See [`TerminalEvent::FocusGained`].
    FocusLost,
}

impl TerminalEvent {
    /// Translates a Crossterm event, returning `None` when it carries nothing
    /// the reducer can act on.
    #[must_use]
    pub fn from_crossterm(event: CrosstermEvent) -> Option<Self> {
        match event {
            CrosstermEvent::Key(key) => KeyPress::from_crossterm(key).map(Self::Key),
            CrosstermEvent::Mouse(mouse) => MouseInput::from_crossterm(mouse).map(Self::Mouse),
            CrosstermEvent::Resize(columns, rows) => Some(Self::Resize { columns, rows }),
            CrosstermEvent::FocusGained => Some(Self::FocusGained),
            CrosstermEvent::FocusLost => Some(Self::FocusLost),
            // Unreachable in practice: the guard never enables bracketed paste.
            // See the module documentation for why it is dropped rather than typed.
            CrosstermEvent::Paste(_) => None,
        }
    }

    /// The key press this event carries, if it is one.
    #[must_use]
    pub const fn as_key(&self) -> Option<KeyPress> {
        match self {
            Self::Key(press) => Some(*press),
            _ => None,
        }
    }
}

/// Everything the reducer consumes (§10.2).
///
/// # The configuration payload
///
/// §10.2 spells the reload variant `ConfigReloaded(Result<Config, ConfigError>)`.
/// Neither type exists yet — configuration is M6 (§21) and lives in the binary
/// crate, which this crate must not depend on (§10.1). Rather than smuggle the
/// payload through `Box<dyn Any>` and downcast it (unsafe-adjacent at runtime,
/// invisible to the type checker, and impossible to match on in a reducer test),
/// the payload is a type parameter. The binary instantiates
/// `Event<Result<Config, ConfigError>>`; this crate's own tests use the default
/// `()`. The reducer never inspects the payload — it forwards it to the config
/// layer — so genericity costs nothing here.
///
/// # Bounded channel
///
/// §10.3 requires the channel carrying these to be bounded and snapshots to be
/// coalesced when the UI falls behind. `Snapshot` therefore holds an
/// [`Arc`]: dropping a superseded snapshot must be cheap and must not free
/// memory the renderer is still reading (§10.4).
///
/// For the same reason [`CollectorHealth`] is boxed even though §10.2 writes it
/// inline: it is by far the largest payload, and an enum is as large as its
/// largest variant, so every queued key press in a bounded channel would
/// otherwise reserve several hundred bytes it never uses (§16.1).
#[derive(Clone, Debug)]
pub enum Event<Cfg = ()> {
    /// Input from the terminal.
    Terminal(TerminalEvent),
    /// A complete, immutable sample (§10.4).
    Snapshot(Arc<SystemSnapshot>),
    /// The answer to an on-demand detail request, including "it vanished".
    Detail(ProcessDetailResult),
    /// A timer tick, carrying monotonic time because §8.1 forbids assuming an
    /// interval and §6.2's multi-key sequences need a timeout.
    Tick(Instant),
    /// A configuration reload finished. See the type-level note above.
    ConfigReloaded(Cfg),
    /// Fresh collector health, which drives the §7.5 diagnostics panel.
    CollectorHealth(Box<CollectorHealth>),
}

impl<Cfg> Event<Cfg> {
    /// A short, allocation-free name, used by trace logging and by the channel's
    /// drop accounting.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Terminal(_) => "terminal",
            Self::Snapshot(_) => "snapshot",
            Self::Detail(_) => "detail",
            Self::Tick(_) => "tick",
            Self::ConfigReloaded(_) => "config-reloaded",
            Self::CollectorHealth(_) => "collector-health",
        }
    }

    /// Whether a queued event may be dropped when the reducer is behind (§10.3).
    ///
    /// Snapshots and ticks are replaceable: a newer one carries strictly better
    /// information. Input, detail results and config reloads are not — dropping a
    /// key press loses the user's intent, and dropping a detail result leaves an
    /// overlay waiting forever.
    #[must_use]
    pub const fn is_coalescable(&self) -> bool {
        matches!(self, Self::Snapshot(_) | Self::Tick(_))
    }

    /// Convenience constructor for a key press event.
    #[must_use]
    pub const fn key(press: KeyPress) -> Self {
        Self::Terminal(TerminalEvent::Key(press))
    }

    /// Convenience constructor for a collector-health event, which hides the box.
    #[must_use]
    pub fn health(health: CollectorHealth) -> Self {
        Self::CollectorHealth(Box::new(health))
    }
}

/// How long the event loop waits for input before emitting a [`Event::Tick`].
///
/// Short enough that a multi-key sequence times out promptly (§6.2) and that
/// `Ctrl-C` is never perceptibly late, long enough that an idle monitrs is not a
/// measurable load itself (§16.1).
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventState, MouseButton as CtMouseButton};

    use super::*;

    fn crossterm_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn key_release_never_produces_a_press() {
        let release = KeyEvent {
            code: KeyCode::Char('K'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };

        assert_eq!(KeyPress::from_crossterm(release), None);
    }

    #[test]
    fn key_repeat_is_treated_as_a_press() {
        let repeat = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        };

        assert_eq!(KeyPress::from_crossterm(repeat), Some(KeyPress::char('j')));
    }

    #[test]
    fn shift_is_dropped_for_character_keys_so_bindings_are_terminal_independent() {
        let with_shift = crossterm_key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let without_shift = crossterm_key(KeyCode::Char('G'), KeyModifiers::NONE);

        assert_eq!(
            KeyPress::from_crossterm(with_shift),
            KeyPress::from_crossterm(without_shift)
        );
        assert_eq!(
            KeyPress::from_crossterm(with_shift),
            Some(KeyPress::char('G'))
        );
    }

    #[test]
    fn upper_and_lower_case_stay_distinct_keys() {
        assert_ne!(KeyPress::char('g'), KeyPress::char('G'));
    }

    #[test]
    fn shift_is_dropped_for_back_tab_which_already_means_shift_tab() {
        let event = crossterm_key(KeyCode::BackTab, KeyModifiers::SHIFT);

        let press = KeyPress::from_crossterm(event).expect("back tab is a modelled key");

        assert_eq!(press, KeyPress::plain(Key::BackTab));
        assert_eq!(press.label(), "Shift-Tab");
    }

    #[test]
    fn ctrl_letters_fold_to_lower_case() {
        let upper = crossterm_key(KeyCode::Char('D'), KeyModifiers::CONTROL);
        let lower = crossterm_key(KeyCode::Char('d'), KeyModifiers::CONTROL);

        assert_eq!(
            KeyPress::from_crossterm(upper),
            KeyPress::from_crossterm(lower)
        );
        assert_eq!(KeyPress::from_crossterm(lower), Some(KeyPress::ctrl('d')));
    }

    #[test]
    fn unmodelled_modifiers_reject_the_press_entirely() {
        let event = crossterm_key(KeyCode::Char('K'), KeyModifiers::SUPER);

        assert_eq!(
            KeyPress::from_crossterm(event),
            None,
            "a Super-modified key must not degrade into plain K (§15.1)"
        );
    }

    #[test]
    fn unmodelled_keys_are_not_bindable() {
        for code in [
            KeyCode::Null,
            KeyCode::CapsLock,
            KeyCode::NumLock,
            KeyCode::ScrollLock,
            KeyCode::PrintScreen,
            KeyCode::Pause,
            KeyCode::Menu,
            KeyCode::KeypadBegin,
        ] {
            assert_eq!(Key::from_crossterm(code), None, "{code:?} must be unbound");
        }
    }

    #[test]
    fn alt_and_ctrl_keys_do_not_type_characters() {
        assert_eq!(KeyPress::ctrl('u').typed_char(), None);
        assert_eq!(
            KeyPress::new(Key::Char('u'), Modifiers::ALT).typed_char(),
            None
        );
        assert_eq!(KeyPress::char('u').typed_char(), Some('u'));
        assert_eq!(KeyPress::char(' ').typed_char(), Some(' '));
        assert_eq!(KeyPress::plain(Key::Enter).typed_char(), None);
    }

    #[test]
    fn key_labels_are_ascii_and_readable() {
        assert_eq!(KeyPress::ctrl('c').label(), "Ctrl-C");
        assert_eq!(KeyPress::ctrl('d').label(), "Ctrl-D");
        assert_eq!(KeyPress::char(' ').label(), "Space");
        assert_eq!(KeyPress::char('G').label(), "G");
        assert_eq!(KeyPress::plain(Key::PageDown).label(), "PageDown");
        assert_eq!(KeyPress::plain(Key::Escape).label(), "Esc");
        assert_eq!(KeyPress::plain(Key::Function(5)).label(), "F5");
        assert!(KeyPress::plain(Key::Home).label().is_ascii());
    }

    #[test]
    fn modifier_sets_report_emptiness() {
        assert!(Modifiers::NONE.is_none());
        assert!(!Modifiers::CTRL.is_none());
        assert!(!Modifiers::ALT.is_none());
        assert!(!Modifiers::SHIFT.is_none());
        assert_eq!(Modifiers::default(), Modifiers::NONE);
        assert_eq!(Modifiers::CTRL.prefix(), "Ctrl-");
        assert_eq!(Modifiers::NONE.prefix(), "");
    }

    #[test]
    fn a_paste_event_is_dropped_rather_than_typed() {
        let event = CrosstermEvent::Paste("rm -rf /".to_owned());

        assert_eq!(
            TerminalEvent::from_crossterm(event),
            None,
            "bracketed paste is never enabled, and unreviewed text must not be inserted"
        );
    }

    #[test]
    fn resize_events_survive_translation() {
        let event = CrosstermEvent::Resize(120, 40);

        assert_eq!(
            TerminalEvent::from_crossterm(event),
            Some(TerminalEvent::Resize {
                columns: 120,
                rows: 40
            })
        );
    }

    #[test]
    fn mouse_scroll_translates_with_position() {
        let event = CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 9,
            modifiers: KeyModifiers::NONE,
        });

        let translated = TerminalEvent::from_crossterm(event).expect("mouse events are modelled");

        assert_eq!(
            translated,
            TerminalEvent::Mouse(MouseInput {
                action: MouseAction::ScrollDown,
                column: 4,
                row: 9,
                modifiers: Modifiers::NONE,
            })
        );
    }

    #[test]
    fn mouse_buttons_translate() {
        assert_eq!(
            MouseButton::from_crossterm(CtMouseButton::Middle),
            MouseButton::Middle
        );
        assert_eq!(
            MouseButton::from_crossterm(CtMouseButton::Right),
            MouseButton::Right
        );
        assert_eq!(
            MouseButton::from_crossterm(CtMouseButton::Left),
            MouseButton::Left
        );
    }

    #[test]
    fn focus_events_translate() {
        assert_eq!(
            TerminalEvent::from_crossterm(CrosstermEvent::FocusGained),
            Some(TerminalEvent::FocusGained)
        );
        assert_eq!(
            TerminalEvent::from_crossterm(CrosstermEvent::FocusLost),
            Some(TerminalEvent::FocusLost)
        );
    }

    #[test]
    fn only_snapshots_and_ticks_may_be_dropped_under_load() {
        let snapshot = Arc::new(SystemSnapshot::warming_up(
            Instant::now(),
            std::time::SystemTime::UNIX_EPOCH,
            8,
        ));

        assert!(Event::<()>::Snapshot(snapshot).is_coalescable());
        assert!(Event::<()>::Tick(Instant::now()).is_coalescable());
        assert!(!Event::<()>::key(KeyPress::char('q')).is_coalescable());
        assert!(!Event::<()>::ConfigReloaded(()).is_coalescable());
        assert!(!Event::<()>::health(CollectorHealth::default()).is_coalescable());
    }

    #[test]
    fn event_kinds_are_distinct_names() {
        let names = [
            Event::<()>::key(KeyPress::char('q')).kind(),
            Event::<()>::Tick(Instant::now()).kind(),
            Event::<()>::ConfigReloaded(()).kind(),
            Event::<()>::health(CollectorHealth::default()).kind(),
        ];

        for (index, name) in names.iter().enumerate() {
            assert!(!name.is_empty());
            assert!(
                !names[index + 1..].contains(name),
                "duplicate event kind name {name}"
            );
        }
    }

    #[test]
    fn as_key_extracts_only_key_events() {
        assert_eq!(
            TerminalEvent::Key(KeyPress::char('q')).as_key(),
            Some(KeyPress::char('q'))
        );
        assert_eq!(
            TerminalEvent::Resize {
                columns: 80,
                rows: 24
            }
            .as_key(),
            None
        );
    }
}
