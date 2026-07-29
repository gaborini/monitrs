//! Terminal rendering for `monitrs`: the visual design system (§5).
//!
//! This crate owns three things that every widget depends on and that are
//! therefore kept free of any rendering logic of their own:
//!
//! * [`glyphs`] — the strict-ASCII / enhanced-Unicode character sets (§5.1). A
//!   widget never writes a literal box-drawing or block character; it asks the
//!   resolved [`GlyphSet`] for one, which is what makes
//!   `--glyphs ascii` provably ASCII-clean.
//! * [`theme`] — semantic color tokens (§5.2, §5.3). A widget never names a
//!   color; it names a meaning. That indirection is what allows the same widget
//!   to render correctly on true color, 256 colors, 16 colors, and with color
//!   switched off entirely.
//! * [`layout`] — breakpoint resolution, panel geometry, and the process-table
//!   column-priority engine (§5.4, §5.7, §7.2). Every rectangle it produces is
//!   inside its parent and no function panics on a zero-area input, which §5.7
//!   makes a hard requirement.
//!
//! The three modules are deliberately independent of `SystemSnapshot`: they
//! describe *how* to draw, never *what*.
//!
//! # The interaction system
//!
//! * [`terminal`] — the RAII guard and the panic hook (§14.3).
//! * [`event`] — terminal, snapshot, detail, and tick events (§10.2).
//! * [`action`] — the `Action` and `Effect` vocabulary (§10.2, §10.5).
//! * [`keymap`] — the default keymap, mode-aware resolution, conflict detection,
//!   and the generated help (§6.1, §6.2, §7.6).
//!
//! # Dependency direction (§10.1)
//!
//! This crate depends on `monitrs-core` and on the terminal libraries, and on
//! nothing else. It cannot know how `/proc` is parsed, and it never performs OS
//! I/O of its own: the reducer returns [`action::Effect`]s and the binary
//! executes them (§10.5). That is what makes keyboard behaviour and the §15.1
//! safety dialogs testable without signalling a real process.
//!
//! # Invariants this crate is responsible for
//!
//! * **The terminal is always restored** — on normal exit, on error, on panic, and
//!   on partial initialization (§14.3, §26).
//! * **No signal from one keypress.** Every keyboard route to a destructive
//!   action produces a proposal that opens a confirmation (§15.1).
//! * **Help is generated from the keymap**, never maintained beside it (§7.6).
//! * **Nothing is printed to stdout or stderr while the alternate screen is
//!   active** (§14.2).
//! * **Colour is never the only indicator.** Every state carries a redundant
//!   symbol (§5.2).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `unwrap`/`expect` stay denied in production code — a panic here corrupts the
// terminal (§14.3). In tests they are the correct way to assert a precondition,
// so the allowance is scoped to `cfg(test)` only (§18.2: narrow allowances).
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod action;
pub mod event;
pub mod glyphs;
pub mod keymap;
pub mod layout;
pub mod terminal;
pub mod theme;
pub mod widgets;

pub use action::{Action, Effect, Effects, Seek, SignalKind, SortField, ViewId};
pub use event::{Event, Key, KeyPress, TerminalEvent};
pub use glyphs::{GlyphMode, GlyphSet, GlyphStyle, TerminalEnv};
pub use keymap::{InputMode, KeyResolver, Keymap};
pub use layout::{Breakpoint, Layout, TableLayout};
pub use terminal::{TerminalGuard, TerminalSettings};
pub use theme::{ColorDepth, ColorMode, Theme, ThemeId, Token};
